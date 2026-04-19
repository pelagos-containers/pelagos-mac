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
use objc2_foundation::NSFileHandle;
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZDiskImageStorageDeviceAttachment,
    VZEntropyDeviceConfiguration, VZFileHandleNetworkDeviceAttachment,
    VZFileHandleSerialPortAttachment, VZGenericPlatformConfiguration, VZLinuxBootLoader,
    VZMACAddress, VZNetworkDeviceConfiguration, VZPlatformConfiguration, VZSerialPortConfiguration,
    VZSharedDirectory, VZSingleDirectoryShare, VZSocketDevice, VZSocketDeviceConfiguration,
    VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioNetworkDeviceConfiguration,
    VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration, VZVirtualMachine,
    VZVirtualMachineConfiguration, VZVirtualMachineState,
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
    /// Additional disk images attached as secondary virtio-blk devices.
    /// First entry → /dev/vdb, second → /dev/vdc, etc.
    pub extra_disks: Vec<PathBuf>,
    /// Number of vCPUs.
    pub cpus: usize,
    /// Memory in MiB.
    pub memory_mib: usize,
    /// vsock port the guest daemon listens on (default 1024).
    pub vsock_port: u32,
    /// Directories to share via virtiofs: (host_path, mount_tag, read_only).
    pub virtiofs_shares: Vec<(PathBuf, String, bool)>,
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
    extra_disks: Vec<PathBuf>,
    cpus: Option<usize>,
    memory_mib: Option<usize>,
    vsock_port: Option<u32>,
    virtiofs_shares: Vec<(PathBuf, String, bool)>,
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
    pub fn extra_disk(mut self, p: impl Into<PathBuf>) -> Self {
        self.extra_disks.push(p.into());
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
    pub fn virtiofs(
        mut self,
        host: impl Into<PathBuf>,
        tag: impl Into<String>,
        read_only: bool,
    ) -> Self {
        self.virtiofs_shares
            .push((host.into(), tag.into(), read_only));
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
            extra_disks: self.extra_disks,
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

/// Static IPv4 address of the VM inside the relay network.
/// Both relay implementations configure the guest with this address.
pub const VM_IP4: &str = "192.168.105.2";

/// A running Linux VM. Holds all AVF resources.
pub struct Vm {
    vm: Arc<SendVm>,
    sock_dev: Arc<SendSockDev>,
    queue: Arc<SendQueue>,
    config: VmConfig,
    /// Keeps the utun relay alive for the VM's lifetime (Drop tears it down).
    _relay: crate::tun_relay::TunRelayHandle,
}

#[derive(Debug)]
pub enum VmExitReason {
    Stopped,
    Error(String),
}

impl Vm {
    /// Boot a VM from the given config. Blocks until the VM reports running state.
    ///
    /// Returns `(Vm, console_fd)` where `console_fd` is the host end of the
    /// serial console socketpair.  Bytes written to `console_fd` reach hvc0
    /// inside the guest; bytes the guest writes to hvc0 are readable from it.
    pub fn start(config: VmConfig) -> Result<(Self, std::os::unix::io::OwnedFd), crate::Error> {
        unsafe { start_vm(config) }
    }

    /// Open a vsock connection to `port` inside the guest.
    ///
    /// Retries up to 30 times with a 1-second delay between attempts to allow
    /// the guest daemon time to start after the VM boots.
    pub fn connect_vsock(&self) -> Result<std::os::unix::io::OwnedFd, crate::Error> {
        // Ping gate worst-case: 15 iterations × -W 3s = 45s.  Give the guest
        // 60 attempts (60s) so the host never times out before pelagos-guest starts.
        const MAX_ATTEMPTS: u32 = 60;
        let mut last_err = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            match self.try_connect_vsock() {
                Ok(fd) => return Ok(fd),
                Err(e) => {
                    last_err = e.to_string();
                    if attempt < MAX_ATTEMPTS {
                        log::debug!("vsock: attempt {}/{}, retrying...", attempt, MAX_ATTEMPTS);
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
                        if fd < 0 {
                            Err(format!("invalid fileDescriptor: {}", fd))
                        } else {
                            // dup() here so we own the fd independently of the
                            // VZVirtioSocketConnection object.  AVF closes the
                            // connection's fd when the ObjC object is deallocated
                            // (ARC), which can happen as soon as this block
                            // returns.  dup() gives us a copy that outlives it.
                            let owned = unsafe { libc::dup(fd) };
                            if owned < 0 {
                                Err(format!("dup failed: {}", std::io::Error::last_os_error()))
                            } else {
                                Ok(owned)
                            }
                        }
                    });
                    c3.notify_one();
                },
            );
            unsafe {
                sock.0.connectToPort_completionHandler(port, &block);
            }
        });

        // Wait up to 5 s for the completion handler to fire.
        // AVF does not call the handler until the guest accepts the connection,
        // so without a timeout this would block indefinitely when the guest
        // daemon is not yet listening.
        let mut g = result.lock().unwrap();
        let timeout = std::time::Duration::from_secs(5);
        let deadline = std::time::Instant::now() + timeout;
        while g.is_none() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(crate::Error::Runtime(
                    "vsock connect timed out (guest not listening yet)".into(),
                ));
            }
            let (guard, _) = cvar.wait_timeout(g, remaining).unwrap();
            g = guard;
        }
        let raw_fd = g.take().unwrap().map_err(crate::Error::Runtime)?;
        Ok(unsafe { std::os::unix::io::OwnedFd::from_raw_fd(raw_fd) })
    }

    /// Add a TCP host→VM port forward via pf RDR rule (pelagos-pfctl).
    ///
    /// Connections to `127.0.0.1:host_port` (and the egress interface) are
    /// kernel-redirected to `VM_IP4:vm_port`.
    pub fn add_port_forward(&self, proto: &str, host_port: u16, vm_port: u16) -> Result<(), crate::Error> {
        self._relay.add_rdr(proto, host_port, VM_IP4, vm_port)
    }

    /// Remove a previously added port forward.
    pub fn remove_port_forward(&self, proto: &str, host_port: u16) -> Result<(), crate::Error> {
        self._relay.remove_rdr(proto, host_port)
    }

    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    /// Request a clean shutdown (ACPI power-off) and wait until the VM
    /// reaches the Stopped state.
    ///
    /// `stopWithCompletionHandler` fires when the *request* is accepted, not
    /// when the VM has actually halted.  The host-side bridge interface
    /// (bridge100) used for NAT only detaches after the VM reaches Stopped.
    /// Returning before that point causes back-to-back VM launches to race
    /// with the previous VM's bridge teardown, producing intermittent NAT
    /// failures in the next VM's init.  We poll `vm.state` after the stop
    /// request completes to ensure bridge100 is fully gone before we return.
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
        g.take().unwrap().map_err(crate::Error::Runtime)?;

        // Poll until the VM reaches Stopped (or Error) state.  The OS tears
        // down bridge100 only after Stopped; this ensures the next VM starts
        // with a clean network slate.
        let queue2 = &self.queue.0;
        loop {
            let vm_clone = Arc::clone(&self.vm);
            let state_holder: Arc<Mutex<VZVirtualMachineState>> =
                Arc::new(Mutex::new(VZVirtualMachineState::Running));
            let state_holder2 = Arc::clone(&state_holder);
            queue2.exec_sync(move || {
                let state = unsafe { vm_clone.0.state() };
                *state_holder2.lock().unwrap() = state;
            });
            let state = *state_holder.lock().unwrap();
            if state == VZVirtualMachineState::Stopped || state == VZVirtualMachineState::Error {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// start_vm — the full AVF initialization sequence
// ---------------------------------------------------------------------------

unsafe fn start_vm(config: VmConfig) -> Result<(Vm, std::os::unix::io::OwnedFd), crate::Error> {
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

    // 3a. Generic platform (required for Linux VMs with VZLinuxBootLoader).
    let platform = VZGenericPlatformConfiguration::new();
    let platform_ref: &VZPlatformConfiguration = &platform;
    vm_config.setPlatform(platform_ref);

    // 4. Virtio block storage.
    // Build all disk configs (root + any extra disks) into a Vec so every
    // Retained handle stays alive across NSArray::from_slice + setStorageDevices.
    // Root disk → /dev/vda; extra disks → /dev/vdb, /dev/vdc, …
    // (Same lifetime pattern as the virtiofs shares section below.)
    let mut block_devs: Vec<Retained<VZVirtioBlockDeviceConfiguration>> = Vec::new();
    for disk_path in std::iter::once(&config.disk).chain(config.extra_disks.iter()) {
        let url = file_url(disk_path);
        let attach = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &url,
            false,
        )
        .map_err(|e| crate::Error::Config(e.localizedDescription().to_string()))?;
        let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attach,
        );
        block_devs.push(blk);
    }
    let storage_refs: Vec<&VZStorageDeviceConfiguration> = block_devs
        .iter()
        .map(|d| d as &VZStorageDeviceConfiguration)
        .collect();
    vm_config.setStorageDevices(&NSArray::from_slice(&storage_refs));

    // 5. Network relay via VZFileHandleNetworkDeviceAttachment.
    //    Returns a SOCK_DGRAM fd that AVF consumes directly as raw Ethernet frames.
    let (vmnet_fd, relay) = crate::tun_relay::start()
        .map_err(|e| crate::Error::Runtime(format!("tun_relay: {e}")))?;
    let vmnet_fh = NSFileHandle::initWithFileDescriptor(NSFileHandle::alloc(), vmnet_fd);
    let net_attach = VZFileHandleNetworkDeviceAttachment::initWithFileHandle(
        VZFileHandleNetworkDeviceAttachment::alloc(),
        &vmnet_fh,
    );
    let net_dev = VZVirtioNetworkDeviceConfiguration::new();
    net_dev.setAttachment(Some(&*net_attach));
    let mac = VZMACAddress::randomLocallyAdministeredAddress();
    net_dev.setMACAddress(&mac);
    log::info!("VM MAC address {}", mac.string());
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
    for (host_path, tag, read_only) in &config.virtiofs_shares {
        let host_url = file_url(host_path);
        let shared_dir = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &host_url,
            *read_only,
        );
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

    // 9. Virtio serial console → guest hvc0, wired to a socketpair.
    //    sp[0] (vm_fd)    → given to VZFileHandleSerialPortAttachment (bidirectional)
    //    sp[1] (relay_fd) → returned to the caller for pelagos vm console relay
    //
    //    We dup vm_fd twice (one for the read handle, one for the write handle)
    //    with closeOnDealloc:true so AVF owns both fds and cleans them up with
    //    the VM.  The original vm_fd is closed after the dups are taken.
    let mut sp = [0i32; 2];
    if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) != 0 {
        return Err(crate::Error::Runtime(format!(
            "socketpair for console: {}",
            std::io::Error::last_os_error()
        )));
    }
    let vm_fd = sp[0];
    let relay_console_fd = std::os::unix::io::OwnedFd::from_raw_fd(sp[1]);
    let read_dup = libc::dup(vm_fd);
    let write_dup = libc::dup(vm_fd);
    libc::close(vm_fd);
    let vm_fh_read =
        NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), read_dup, true);
    let vm_fh_write =
        NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), write_dup, true);
    let serial_attach =
        VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
            VZFileHandleSerialPortAttachment::alloc(),
            Some(&*vm_fh_read),
            Some(&*vm_fh_write),
        );
    let serial_port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
    serial_port.setAttachment(Some(&*serial_attach));
    let serial_ref: &VZSerialPortConfiguration = &serial_port;
    vm_config.setSerialPorts(&NSArray::from_slice(&[serial_ref]));

    // 10. Validate.
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
                let e = unsafe { &*err };
                let desc = e.localizedDescription();
                let domain = e.domain();
                let code = e.code();
                let reason = e
                    .localizedFailureReason()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                Err(format!(
                    "[{} {}] {} | reason: {}",
                    domain, code, desc, reason
                ))
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

    Ok((
        Vm {
            vm: vm_arc,
            sock_dev: Arc::new(SendSockDev(sock_dev)),
            queue: queue_arc,
            config,
            _relay: relay,
        },
        relay_console_fd,
    ))
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
