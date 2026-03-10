//! VM lifecycle management via Apple Virtualization Framework.
//!
//! `VmConfig` describes what to boot; `Vm` owns the running VM instance.
//! All AVF calls are serialized through a private serial dispatch queue.
//! vsock connections from host→guest are in-process via `VZVirtioSocketDevice`.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{rc::Retained, AnyThread};
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZDiskImageStorageDeviceAttachment,
    VZEntropyDeviceConfiguration, VZLinuxBootLoader, VZNATNetworkDeviceAttachment,
    VZNetworkDeviceConfiguration, VZSharedDirectory, VZSingleDirectoryShare, VZSocketDevice,
    VZSocketDeviceConfiguration, VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioEntropyDeviceConfiguration, VZVirtioFileSystemDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration,
    VZVirtualMachine, VZVirtualMachineConfiguration,
};
use std::os::fd::FromRawFd;

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

/// Configuration for a Linux VM.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Path to the Linux kernel image (uncompressed or gzip).
    pub kernel: PathBuf,
    /// Path to the initial ramdisk.
    pub initrd: Option<PathBuf>,
    /// Kernel command-line arguments.
    pub cmdline: String,
    /// Root disk image (raw).
    pub disk: PathBuf,
    /// Number of vCPUs.
    pub cpus: usize,
    /// Memory in MiB.
    pub memory_mib: usize,
    /// vsock port the guest daemon listens on (default 1024).
    pub vsock_port: u32,
    /// Directories to share via virtiofs: (host_path, mount_tag).
    pub virtiofs_shares: Vec<(PathBuf, String)>,
    /// Enable Rosetta for x86_64 Linux binaries (macOS 13+).
    pub rosetta: bool,
}

impl VmConfig {
    pub fn builder() -> VmConfigBuilder {
        VmConfigBuilder::default()
    }
}

#[derive(Default)]
pub struct VmConfigBuilder {
    kernel: Option<PathBuf>,
    initrd: Option<PathBuf>,
    cmdline: Option<String>,
    disk: Option<PathBuf>,
    cpus: Option<usize>,
    memory_mib: Option<usize>,
    vsock_port: Option<u32>,
    virtiofs_shares: Vec<(PathBuf, String)>,
    rosetta: bool,
}

impl VmConfigBuilder {
    pub fn kernel(mut self, p: impl Into<PathBuf>) -> Self {
        self.kernel = Some(p.into());
        self
    }
    pub fn initrd(mut self, p: impl Into<PathBuf>) -> Self {
        self.initrd = Some(p.into());
        self
    }
    pub fn cmdline(mut self, s: impl Into<String>) -> Self {
        self.cmdline = Some(s.into());
        self
    }
    pub fn disk(mut self, p: impl Into<PathBuf>) -> Self {
        self.disk = Some(p.into());
        self
    }
    pub fn cpus(mut self, n: usize) -> Self {
        self.cpus = Some(n);
        self
    }
    pub fn memory_mib(mut self, n: usize) -> Self {
        self.memory_mib = Some(n);
        self
    }
    pub fn vsock_port(mut self, p: u32) -> Self {
        self.vsock_port = Some(p);
        self
    }
    pub fn virtiofs(mut self, host: impl Into<PathBuf>, tag: impl Into<String>) -> Self {
        self.virtiofs_shares.push((host.into(), tag.into()));
        self
    }
    pub fn rosetta(mut self, enabled: bool) -> Self {
        self.rosetta = enabled;
        self
    }

    pub fn build(self) -> Result<VmConfig, &'static str> {
        Ok(VmConfig {
            kernel: self.kernel.ok_or("kernel required")?,
            initrd: self.initrd,
            cmdline: self
                .cmdline
                .unwrap_or_else(|| "console=hvc0 root=/dev/vda rw".into()),
            disk: self.disk.ok_or("disk required")?,
            cpus: self.cpus.unwrap_or(2),
            memory_mib: self.memory_mib.unwrap_or(1024),
            vsock_port: self.vsock_port.unwrap_or(1024),
            virtiofs_shares: self.virtiofs_shares,
            rosetta: self.rosetta,
        })
    }
}

// ---------------------------------------------------------------------------
// Thread-safety wrappers
//
// VZVirtualMachine and VZVirtioSocketDevice are ObjC objects: objc2's
// Retained<T> is !Send by default. We assert Send+Sync here because:
//   - All AVF method calls are dispatched through the VM's serial queue.
//   - The serial queue serializes access; no concurrent method calls occur.
// ---------------------------------------------------------------------------

struct SendVm(Retained<VZVirtualMachine>);
// Safety: all VZVirtualMachine access goes through the designated serial queue.
unsafe impl Send for SendVm {}
unsafe impl Sync for SendVm {}

struct SendSockDev(Retained<VZVirtioSocketDevice>);
// Safety: same queue serialization guarantee as SendVm.
unsafe impl Send for SendSockDev {}
unsafe impl Sync for SendSockDev {}

type DQueue = dispatch2::DispatchRetained<dispatch2::DispatchQueue>;

struct SendQueue(DQueue);
// Safety: DispatchQueue is thread-safe by design.
unsafe impl Send for SendQueue {}
unsafe impl Sync for SendQueue {}

// ---------------------------------------------------------------------------
// Vm
// ---------------------------------------------------------------------------

/// A running Linux VM. Holds all AVF resources.
pub struct Vm {
    vm: Arc<SendVm>,
    sock_dev: Arc<SendSockDev>,
    queue: Arc<SendQueue>,
    config: VmConfig,
}

#[derive(Debug)]
pub enum VmExitReason {
    Stopped,
    Error(String),
}

impl Vm {
    /// Boot a VM from the given config. Blocks until the VM reports running state.
    pub fn start(config: VmConfig) -> Result<Self, crate::Error> {
        unsafe { start_vm(config) }
    }

    /// Open a vsock connection to `port` inside the guest.
    ///
    /// Retries up to 30 times with a 1-second delay between attempts to allow
    /// the guest daemon time to start after the VM boots.
    pub fn connect_vsock(&self) -> Result<std::os::unix::io::OwnedFd, crate::Error> {
        const MAX_ATTEMPTS: u32 = 30;
        let mut last_err = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            match self.try_connect_vsock() {
                Ok(fd) => return Ok(fd),
                Err(e) => {
                    last_err = e.to_string();
                    if attempt < MAX_ATTEMPTS {
                        eprintln!("vsock: attempt {}/{}, retrying...", attempt, MAX_ATTEMPTS);
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        }
        Err(crate::Error::Runtime(last_err))
    }

    /// Single vsock connection attempt; called by `connect_vsock`.
    fn try_connect_vsock(&self) -> Result<std::os::unix::io::OwnedFd, crate::Error> {
        let port = self.config.vsock_port;
        let sock = Arc::clone(&self.sock_dev);
        let queue = &self.queue.0;

        let result: Arc<Mutex<Option<Result<i32, String>>>> = Arc::new(Mutex::new(None));
        let cvar = Arc::new(Condvar::new());
        let r2 = Arc::clone(&result);
        let c2 = Arc::clone(&cvar);

        queue.exec_sync(move || {
            let r3 = Arc::clone(&r2);
            let c3 = Arc::clone(&c2);
            let block = RcBlock::new(
                move |conn: *mut objc2_virtualization::VZVirtioSocketConnection,
                      err: *mut NSError| {
                    let mut g = r3.lock().unwrap();
                    *g = Some(if !err.is_null() {
                        let desc = unsafe { &*err }.localizedDescription();
                        Err(desc.to_string())
                    } else if conn.is_null() {
                        Err("null connection".into())
                    } else {
                        let fd = unsafe { (&*conn).fileDescriptor() };
                        Ok(fd)
                    });
                    c3.notify_one();
                },
            );
            unsafe {
                sock.0.connectToPort_completionHandler(port, &block);
            }
        });

        let mut g = result.lock().unwrap();
        while g.is_none() {
            g = cvar.wait(g).unwrap();
        }
        let raw_fd = g.take().unwrap().map_err(crate::Error::Runtime)?;
        Ok(unsafe { std::os::unix::io::OwnedFd::from_raw_fd(raw_fd) })
    }

    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    /// Request a clean shutdown (ACPI power-off).
    pub fn stop(&self) -> Result<(), crate::Error> {
        let vm = Arc::clone(&self.vm);
        let queue = &self.queue.0;
        let result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
        let cvar = Arc::new(Condvar::new());
        let r2 = Arc::clone(&result);
        let c2 = Arc::clone(&cvar);

        queue.exec_sync(move || {
            let r3 = Arc::clone(&r2);
            let c3 = Arc::clone(&c2);
            let block = RcBlock::new(move |err: *mut NSError| {
                let mut g = r3.lock().unwrap();
                *g = Some(if err.is_null() {
                    Ok(())
                } else {
                    let desc = unsafe { &*err }.localizedDescription();
                    Err(desc.to_string())
                });
                c3.notify_one();
            });
            unsafe {
                vm.0.stopWithCompletionHandler(&block);
            }
        });

        let mut g = result.lock().unwrap();
        while g.is_none() {
            g = cvar.wait(g).unwrap();
        }
        g.take().unwrap().map_err(crate::Error::Runtime)
    }
}

// ---------------------------------------------------------------------------
// start_vm — the full AVF initialization sequence
// ---------------------------------------------------------------------------

unsafe fn start_vm(config: VmConfig) -> Result<Vm, crate::Error> {
    // 1. Serial dispatch queue — all AVF method calls go through this queue.
    let queue = DispatchQueue::new("com.pelagos.vm", DispatchQueueAttr::SERIAL);

    // 2. Linux boot loader.
    let kernel_url = file_url(&config.kernel);
    let bootloader = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url);
    bootloader.setCommandLine(&make_nsstring(&config.cmdline));
    if let Some(ref initrd_path) = config.initrd {
        bootloader.setInitialRamdiskURL(Some(&file_url(initrd_path)));
    }

    // 3. VM configuration.
    let vm_config = VZVirtualMachineConfiguration::new();
    vm_config.setBootLoader(Some(&*bootloader));
    vm_config.setCPUCount(config.cpus);
    vm_config.setMemorySize((config.memory_mib as u64) * 1024 * 1024);

    // 4. Virtio block storage.
    let disk_url = file_url(&config.disk);
    let disk_attach = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
        VZDiskImageStorageDeviceAttachment::alloc(),
        &disk_url,
        false,
    )
    .map_err(|e| crate::Error::Config(e.localizedDescription().to_string()))?;
    let block_dev = VZVirtioBlockDeviceConfiguration::initWithAttachment(
        VZVirtioBlockDeviceConfiguration::alloc(),
        &disk_attach,
    );
    let storage_ref: &VZStorageDeviceConfiguration = &block_dev;
    vm_config.setStorageDevices(&NSArray::from_slice(&[storage_ref]));

    // 5. Virtio NAT network.
    let nat = VZNATNetworkDeviceAttachment::new();
    let net_dev = VZVirtioNetworkDeviceConfiguration::new();
    net_dev.setAttachment(Some(&*nat));
    let net_ref: &VZNetworkDeviceConfiguration = &net_dev;
    vm_config.setNetworkDevices(&NSArray::from_slice(&[net_ref]));

    // 6. Virtio entropy (guest /dev/random).
    let entropy = VZVirtioEntropyDeviceConfiguration::new();
    let ent_ref: &VZEntropyDeviceConfiguration = &entropy;
    vm_config.setEntropyDevices(&NSArray::from_slice(&[ent_ref]));

    // 7. Virtio vsock device.
    let vsock_dev = VZVirtioSocketDeviceConfiguration::new();
    let sock_ref: &VZSocketDeviceConfiguration = &vsock_dev;
    vm_config.setSocketDevices(&NSArray::from_slice(&[sock_ref]));

    // 8. virtiofs directory shares.
    let mut fs_configs: Vec<Retained<VZVirtioFileSystemDeviceConfiguration>> = Vec::new();
    for (host_path, tag) in &config.virtiofs_shares {
        let host_url = file_url(host_path);
        let shared_dir =
            VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &host_url, false);
        let share =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &shared_dir);
        let fs_config = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &make_nsstring(tag),
        );
        fs_config.setShare(Some(&*share));
        fs_configs.push(fs_config);
    }
    if !fs_configs.is_empty() {
        let refs: Vec<&VZDirectorySharingDeviceConfiguration> =
            fs_configs.iter().map(|c| c.as_ref()).collect();
        vm_config.setDirectorySharingDevices(&NSArray::from_slice(&refs));
    }

    // 9. Validate.
    vm_config
        .validateWithError()
        .map_err(|e| crate::Error::Config(e.localizedDescription().to_string()))?;

    // 10. Create VM with our designated serial queue.
    let vm = VZVirtualMachine::initWithConfiguration_queue(
        VZVirtualMachine::alloc(),
        &vm_config,
        &queue,
    );

    // 11. Start: dispatch the startWithCompletionHandler call to the VM's queue,
    //     then block the calling thread on a condvar until the callback fires.
    let vm_arc = Arc::new(SendVm(vm));
    let vm_for_start = Arc::clone(&vm_arc);

    let start_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let cvar = Arc::new(Condvar::new());
    let r2 = Arc::clone(&start_result);
    let c2 = Arc::clone(&cvar);

    queue.exec_sync(move || {
        let r3 = Arc::clone(&r2);
        let c3 = Arc::clone(&c2);
        let block = RcBlock::new(move |err: *mut NSError| {
            let mut g = r3.lock().unwrap();
            *g = Some(if err.is_null() {
                Ok(())
            } else {
                let desc = unsafe { &*err }.localizedDescription();
                Err(desc.to_string())
            });
            c3.notify_one();
        });
        vm_for_start.0.startWithCompletionHandler(&block);
    });

    // Wait for the start completion callback.
    let mut g = start_result.lock().unwrap();
    while g.is_none() {
        g = cvar.wait(g).unwrap();
    }
    g.take().unwrap().map_err(crate::Error::Runtime)?;

    // 12. Retrieve the vsock device from the running VM.
    // exec_sync returns (), so we shuttle the result via Arc<Mutex<Option<_>>>.
    struct SendSockOpt(Option<Retained<VZVirtioSocketDevice>>);
    unsafe impl Send for SendSockOpt {}

    let sock_holder: Arc<Mutex<SendSockOpt>> = Arc::new(Mutex::new(SendSockOpt(None)));
    let sock_holder2 = Arc::clone(&sock_holder);
    let vm_for_sock = Arc::clone(&vm_arc);

    queue.exec_sync(move || {
        let devices = vm_for_sock.0.socketDevices();
        // Safety: we configured exactly one VZVirtioSocketDeviceConfiguration so the
        // runtime device is a VZVirtioSocketDevice.
        let first: &VZSocketDevice = &devices.objectAtIndex(0);
        let ptr = first as *const VZSocketDevice as *mut VZVirtioSocketDevice;
        if let Some(sock) = unsafe { Retained::retain(ptr) } {
            sock_holder2.lock().unwrap().0 = Some(sock);
        }
    });

    let sock_dev = sock_holder
        .lock()
        .unwrap()
        .0
        .take()
        .ok_or_else(|| crate::Error::Runtime("failed to obtain vsock device".into()))?;

    let queue_arc = Arc::new(SendQueue(queue));

    Ok(Vm {
        vm: vm_arc,
        sock_dev: Arc::new(SendSockDev(sock_dev)),
        queue: queue_arc,
        config,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    let s = path.to_str().expect("non-UTF8 path");
    NSURL::initFileURLWithPath(NSURL::alloc(), &make_nsstring(s))
}

fn make_nsstring(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

impl Drop for Vm {
    fn drop(&mut self) {
        // Best-effort stop; ignore errors during drop.
        let _ = self.stop();
    }
}
