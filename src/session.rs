use std::convert::TryFrom;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "async-std-runtime")]
use async_std::fs::read_dir;
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_util::future::FutureExt;
use futures_util::sink::{Sink, SinkExt};
use futures_util::stream::StreamExt;
use futures_util::{pin_mut, select};
use log::{debug, error, warn};
use nix::mount;
use nix::mount::MsFlags;
#[cfg(all(not(feature = "async-std-runtime"), feature = "tokio-runtime"))]
use tokio::fs::read_dir;

use lazy_static::lazy_static;

use crate::abi::*;
#[cfg(any(feature = "async-std-runtime", feature = "tokio-runtime"))]
use crate::connection::FuseConnection;
use crate::filesystem::Filesystem;
use crate::helper::*;
use crate::notify::Notify;
use crate::reply::ReplyXAttr;
use crate::request::Request;
use crate::spawn::spawn_without_return;
use crate::MountOptions;
use crate::{Errno, SetAttr};

lazy_static! {
    static ref BINARY: bincode::Config = {
        let mut cfg = bincode::config();
        cfg.little_endian();

        cfg
    };
}

#[cfg(any(feature = "async-std-runtime", feature = "tokio-runtime"))]
/// fuse filesystem session.
pub struct Session<FS> {
    fuse_connection: Option<Arc<FuseConnection>>,
    filesystem: Option<Arc<FS>>,
    response_sender: UnboundedSender<Vec<u8>>,
    response_receiver: Option<UnboundedReceiver<Vec<u8>>>,
    mount_options: MountOptions,
}

#[cfg(any(feature = "async-std-runtime", feature = "tokio-runtime"))]
impl<FS> Session<FS> {
    /// new a fuse filesystem session.
    pub fn new(mount_options: MountOptions) -> Self {
        let (sender, receiver) = unbounded();

        Self {
            fuse_connection: None,
            filesystem: None,
            response_sender: sender,
            response_receiver: Some(receiver),
            mount_options,
        }
    }

    /// get a [`notify`].
    ///
    /// [`notify`]: Notify
    pub fn get_notify(&self) -> Notify {
        Notify::new(self.response_sender.clone())
    }
}

#[cfg(any(feature = "async-std-runtime", feature = "tokio-runtime"))]
impl<FS: Filesystem + Send + Sync + 'static> Session<FS> {
    #[cfg(feature = "unprivileged")]
    /// mount the filesystem without root permission. This function will block until the filesystem
    /// is unmounted.
    pub async fn mount_with_unprivileged<P: AsRef<Path>>(
        mut self,
        fs: FS,
        mount_path: P,
    ) -> IoResult<()> {
        if !self.mount_options.nonempty
            && read_dir(mount_path.as_ref()).await?.next().await.is_some()
        {
            return Err(IoError::new(
                ErrorKind::AlreadyExists,
                "mount point is not empty",
            ));
        }

        let fuse_connection =
            FuseConnection::new_with_unprivileged(self.mount_options.clone(), mount_path.as_ref())
                .await?;

        self.fuse_connection.replace(Arc::new(fuse_connection));

        self.filesystem.replace(Arc::new(fs));

        debug!("mount {:?} success", mount_path.as_ref());

        self.inner_mount().await
    }

    /// mount the filesystem. This function will block until the filesystem is unmounted.
    pub async fn mount<P: AsRef<Path>>(mut self, fs: FS, mount_path: P) -> IoResult<()> {
        let mut mount_options = self.mount_options.clone();

        if !mount_options.nonempty && read_dir(mount_path.as_ref()).await?.next().await.is_some() {
            return Err(IoError::new(
                ErrorKind::AlreadyExists,
                "mount point is not empty",
            ));
        }

        let fuse_connection = FuseConnection::new().await?;

        let fd = fuse_connection.as_raw_fd();

        let options = mount_options.build(fd);

        let fs_name = if let Some(fs_name) = mount_options.fs_name.as_ref() {
            Some(fs_name.as_str())
        } else {
            Some("fuse")
        };

        debug!("mount options {:?}", options);

        if let Err(err) = mount::mount(
            fs_name,
            mount_path.as_ref(),
            Some("fuse"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some(options.as_os_str()),
        ) {
            error!("mount {:?} failed", mount_path.as_ref());

            return Err(io_error_from_nix_error(err));
        }

        self.fuse_connection.replace(Arc::new(fuse_connection));

        self.filesystem.replace(Arc::new(fs));

        debug!("mount {:?} success", mount_path.as_ref());

        self.inner_mount().await
    }

    async fn inner_mount(&mut self) -> IoResult<()> {
        let fuse_write_connection = self.fuse_connection.as_ref().unwrap().clone();

        let receiver = self.response_receiver.take().unwrap();

        let dispatch_task = self.dispatch().fuse();

        pin_mut!(dispatch_task);

        #[cfg(feature = "async-std-runtime")]
        {
            let reply_task = async_std::task::spawn(async move {
                Self::reply_fuse(fuse_write_connection, receiver).await
            })
            .fuse();

            pin_mut!(reply_task);

            select! {
                reply_result = reply_task => {
                    if let Err(err) = reply_result {
                        return Err(err)
                    }
                }

                dispatch_result = dispatch_task => {
                    if let Err(err) = dispatch_result {
                        return Err(err)
                    }
                }
            }
        }

        #[cfg(all(not(feature = "async-std-runtime"), feature = "tokio-runtime"))]
        {
            let reply_task =
                tokio::spawn(
                    async move { Self::reply_fuse(fuse_write_connection, receiver).await },
                )
                .fuse();

            pin_mut!(reply_task);

            select! {
                reply_result = reply_task => {
                    if let Err(err) = reply_result.unwrap() {
                        return Err(err)
                    }
                }

                dispatch_result = dispatch_task => {
                    if let Err(err) = dispatch_result {
                        return Err(err)
                    }
                }
            }
        }

        Ok(())
    }

    async fn reply_fuse(
        fuse_connection: Arc<FuseConnection>,
        mut response_receiver: UnboundedReceiver<Vec<u8>>,
    ) -> IoResult<()> {
        while let Some(response) = response_receiver.next().await {
            let n = response.len();

            if let Err((_, err)) = fuse_connection.write(response, n).await {
                if err.kind() == ErrorKind::NotFound {
                    warn!(
                        "may reply interrupted fuse request, ignore this error {}",
                        err
                    );

                    continue;
                }

                error!("reply fuse failed {}", err);

                return Err(err);
            }
        }

        Ok(())
    }

    async fn dispatch(&mut self) -> IoResult<()> {
        let mut buffer = vec![0; BUFFER_SIZE];

        let fuse_connection = self.fuse_connection.take().unwrap();

        let fs = self.filesystem.take().expect("filesystem not init");

        'dispatch_loop: loop {
            let mut data = match fuse_connection.read(buffer).await {
                Err((_, err)) => {
                    if let Some(errno) = err.raw_os_error() {
                        if errno == libc::ENODEV {
                            debug!("read from /dev/fuse failed with ENODEV, call destroy now");

                            fs.destroy(Request {
                                unique: 0,
                                uid: 0,
                                gid: 0,
                                pid: 0,
                            })
                            .await;

                            return Ok(());
                        }
                    }

                    error!("read from /dev/fuse failed {}", err);

                    return Err(err);
                }

                Ok((buf, n)) => {
                    buffer = buf;

                    &buffer[..n]
                }
            };

            let in_header = match BINARY.deserialize::<fuse_in_header>(data) {
                Err(err) => {
                    error!("deserialize fuse_in_header failed {}", err);

                    continue;
                }

                Ok(in_header) => in_header,
            };

            let request = Request::from(&in_header);

            let opcode = match fuse_opcode::try_from(in_header.opcode) {
                Err(err) => {
                    debug!("receive unknown opcode {}", err.0);

                    reply_error(libc::ENOSYS.into(), request, self.response_sender.clone());

                    continue;
                }
                Ok(opcode) => opcode,
            };

            debug!("receive opcode {}", opcode);

            // data = &data[FUSE_IN_HEADER_SIZE..in_header.len as usize - FUSE_IN_HEADER_SIZE];
            data = &data[FUSE_IN_HEADER_SIZE..];
            data = &data[..in_header.len as usize - FUSE_IN_HEADER_SIZE];

            match opcode {
                fuse_opcode::FUSE_INIT => {
                    let init_in = match BINARY.deserialize::<fuse_init_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_init_in failed {}, request unique {}",
                                err, request.unique
                            );

                            let init_out_header = fuse_out_header {
                                len: FUSE_OUT_HEADER_SIZE as u32,
                                error: libc::EINVAL,
                                unique: request.unique,
                            };

                            let init_out_header_data =
                                BINARY.serialize(&init_out_header).expect("won't happened");

                            if let Err((_, err)) = fuse_connection
                                .write(init_out_header_data, FUSE_OUT_HEADER_SIZE)
                                .await
                            {
                                error!("write error init out data to /dev/fuse failed {}", err);
                            }

                            return Err(IoError::from_raw_os_error(libc::EINVAL));
                        }

                        Ok(init_in) => init_in,
                    };

                    debug!("fuse_init {:?}", init_in);

                    let mut reply_flags = 0;

                    if init_in.flags & FUSE_ASYNC_READ > 0 {
                        debug!("enable FUSE_ASYNC_READ");

                        reply_flags |= FUSE_ASYNC_READ;
                    }

                    #[cfg(feature = "file-lock")]
                    if init_in.flags & FUSE_POSIX_LOCKS > 0 {
                        debug!("enable FUSE_POSIX_LOCKS");

                        reply_flags |= FUSE_POSIX_LOCKS;
                    }

                    if init_in.flags & FUSE_FILE_OPS > 0 {
                        debug!("enable FUSE_FILE_OPS");

                        reply_flags |= FUSE_FILE_OPS;
                    }

                    if init_in.flags & FUSE_ATOMIC_O_TRUNC > 0 {
                        debug!("enable FUSE_ATOMIC_O_TRUNC");

                        reply_flags |= FUSE_ATOMIC_O_TRUNC;
                    }

                    if init_in.flags & FUSE_EXPORT_SUPPORT > 0 {
                        debug!("enable FUSE_EXPORT_SUPPORT");

                        reply_flags |= FUSE_EXPORT_SUPPORT;
                    }

                    if init_in.flags & FUSE_BIG_WRITES > 0 {
                        debug!("enable FUSE_BIG_WRITES");

                        reply_flags |= FUSE_BIG_WRITES;
                    }

                    if init_in.flags & FUSE_DONT_MASK > 0 && self.mount_options.dont_mask {
                        debug!("enable FUSE_DONT_MASK");

                        reply_flags |= FUSE_DONT_MASK;
                    }

                    if init_in.flags & FUSE_SPLICE_WRITE > 0 {
                        debug!("enable FUSE_SPLICE_WRITE");

                        reply_flags |= FUSE_SPLICE_WRITE;
                    }

                    if init_in.flags & FUSE_SPLICE_MOVE > 0 {
                        debug!("enable FUSE_SPLICE_MOVE");

                        reply_flags |= FUSE_SPLICE_MOVE;
                    }

                    if init_in.flags & FUSE_SPLICE_READ > 0 {
                        debug!("enable FUSE_SPLICE_READ");

                        reply_flags |= FUSE_SPLICE_READ;
                    }

                    // posix lock used, maybe we don't need bsd lock
                    /*if init_in.flags&FUSE_FLOCK_LOCKS>0 {
                        reply_flags |= FUSE_FLOCK_LOCKS;
                    }*/

                    /*if init_in.flags & FUSE_HAS_IOCTL_DIR > 0 {
                        debug!("enable FUSE_HAS_IOCTL_DIR");

                        reply_flags |= FUSE_HAS_IOCTL_DIR;
                    }*/

                    if init_in.flags & FUSE_AUTO_INVAL_DATA > 0 {
                        debug!("enable FUSE_AUTO_INVAL_DATA");

                        reply_flags |= FUSE_AUTO_INVAL_DATA;
                    }

                    if init_in.flags & FUSE_DO_READDIRPLUS > 0
                        || self.mount_options.force_readdir_plus
                    {
                        debug!("enable FUSE_DO_READDIRPLUS");

                        reply_flags |= FUSE_DO_READDIRPLUS;
                    }

                    if init_in.flags & FUSE_READDIRPLUS_AUTO > 0
                        && !self.mount_options.force_readdir_plus
                    {
                        debug!("enable FUSE_READDIRPLUS_AUTO");

                        reply_flags |= FUSE_READDIRPLUS_AUTO;
                    }

                    if init_in.flags & FUSE_ASYNC_DIO > 0 {
                        debug!("enable FUSE_ASYNC_DIO");

                        reply_flags |= FUSE_ASYNC_DIO;
                    }

                    if init_in.flags & FUSE_WRITEBACK_CACHE > 0 && self.mount_options.write_back {
                        debug!("enable FUSE_WRITEBACK_CACHE");

                        reply_flags |= FUSE_WRITEBACK_CACHE;
                    }

                    if init_in.flags & FUSE_NO_OPEN_SUPPORT > 0
                        && self.mount_options.no_open_support
                    {
                        debug!("enable FUSE_NO_OPEN_SUPPORT");

                        reply_flags |= FUSE_NO_OPEN_SUPPORT;
                    }

                    if init_in.flags & FUSE_PARALLEL_DIROPS > 0 {
                        debug!("enable FUSE_PARALLEL_DIROPS");

                        reply_flags |= FUSE_PARALLEL_DIROPS;
                    }

                    if init_in.flags & FUSE_HANDLE_KILLPRIV > 0
                        && self.mount_options.handle_killpriv
                    {
                        debug!("enable FUSE_HANDLE_KILLPRIV");

                        reply_flags |= FUSE_HANDLE_KILLPRIV;
                    }

                    if init_in.flags & FUSE_POSIX_ACL > 0 && self.mount_options.default_permissions
                    {
                        debug!("enable FUSE_POSIX_ACL");

                        reply_flags |= FUSE_POSIX_ACL;
                    }

                    if init_in.flags & FUSE_MAX_PAGES > 0 {
                        debug!("enable FUSE_MAX_PAGES");

                        reply_flags |= FUSE_MAX_PAGES;
                    }

                    if init_in.flags & FUSE_CACHE_SYMLINKS > 0 {
                        debug!("enable FUSE_CACHE_SYMLINKS");

                        reply_flags |= FUSE_CACHE_SYMLINKS;
                    }

                    if init_in.flags & FUSE_NO_OPENDIR_SUPPORT > 0
                        && self.mount_options.no_open_dir_support
                    {
                        debug!("enable FUSE_NO_OPENDIR_SUPPORT");

                        reply_flags |= FUSE_NO_OPENDIR_SUPPORT;
                    }

                    if let Err(err) = fs.init(request).await {
                        let init_out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: err.into(),
                            unique: request.unique,
                        };

                        let init_out_header_data =
                            BINARY.serialize(&init_out_header).expect("won't happened");

                        if let Err((_, err)) = fuse_connection
                            .write(init_out_header_data, FUSE_OUT_HEADER_SIZE)
                            .await
                        {
                            error!("write error init out data to /dev/fuse failed {}", err);
                        }

                        return Err(IoError::from_raw_os_error(err.0));
                    }

                    let init_out = fuse_init_out {
                        major: FUSE_KERNEL_VERSION,
                        minor: FUSE_KERNEL_MINOR_VERSION,
                        max_readahead: init_in.max_readahead,
                        flags: reply_flags,
                        max_background: DEFAULT_MAX_BACKGROUND,
                        congestion_threshold: DEFAULT_CONGESTION_THRESHOLD,
                        max_write: MAX_WRITE_SIZE as u32,
                        time_gran: DEFAULT_TIME_GRAN,
                        max_pages: DEFAULT_MAX_PAGES,
                        map_alignment: DEFAULT_MAP_ALIGNMENT,
                        unused: [0; 8],
                    };

                    debug!("fuse init out {:?}", init_out);

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_INIT_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_INIT_OUT_SIZE);

                    BINARY
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    BINARY
                        .serialize_into(&mut data, &init_out)
                        .expect("won't happened");

                    if let Err((_, err)) = fuse_connection
                        .write(data, FUSE_OUT_HEADER_SIZE + FUSE_INIT_OUT_SIZE)
                        .await
                    {
                        error!("write init out data to /dev/fuse failed {}", err);

                        return Err(err);
                    }

                    debug!("fuse init done");
                }

                fuse_opcode::FUSE_DESTROY => {
                    debug!("receive fuse destroy");

                    fs.destroy(request).await;

                    debug!("fuse destroyed");

                    return Ok(());
                }

                fuse_opcode::FUSE_LOOKUP => {
                    let mut resp_sender = self.response_sender.clone();

                    let name = match get_first_null_position(data) {
                        None => {
                            error!("lookup body has no null, request unique {}", request.unique);

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "lookup unique {} name {:?} in parent {}",
                            request.unique, name, in_header.nodeid
                        );

                        let data = match fs.lookup(request, in_header.nodeid, &name).await {
                            Err(err) => {
                                let out_header = fuse_out_header {
                                    len: FUSE_OUT_HEADER_SIZE as u32,
                                    error: err.into(),
                                    unique: request.unique,
                                };

                                BINARY.serialize(&out_header).expect("won't happened")
                            }

                            Ok(entry) => {
                                let entry_out: fuse_entry_out = entry.into();

                                debug!("lookup response {:?}", entry_out);

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &entry_out)
                                    .expect("won't happened");

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_FORGET => {
                    let forget_in = match BINARY.deserialize::<fuse_forget_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_forget_in failed {}, request unique {}",
                                err, request.unique
                            );

                            // no need to reply
                            continue;
                        }

                        Ok(forget_in) => forget_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "forget unique {} inode {} nlookup {}",
                            request.unique, in_header.nodeid, forget_in.nlookup
                        );

                        fs.forget(request, in_header.nodeid, forget_in.nlookup)
                            .await
                    });
                }

                fuse_opcode::FUSE_GETATTR => {
                    let mut resp_sender = self.response_sender.clone();

                    let getattr_in = match BINARY.deserialize::<fuse_getattr_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_forget_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;

                            continue;
                        }

                        Ok(getattr_in) => getattr_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "getattr unique {} inode {}",
                            request.unique, in_header.nodeid
                        );

                        let fh = if getattr_in.getattr_flags & FUSE_GETATTR_FH > 0 {
                            Some(getattr_in.fh)
                        } else {
                            None
                        };

                        let data = match fs
                            .getattr(request, in_header.nodeid, fh, getattr_in.getattr_flags)
                            .await
                        {
                            Err(err) => {
                                let out_header = fuse_out_header {
                                    len: FUSE_OUT_HEADER_SIZE as u32,
                                    error: err.into(),
                                    unique: request.unique,
                                };

                                BINARY.serialize(&out_header).expect("won't happened")
                            }

                            Ok(attr) => {
                                let attr_out = fuse_attr_out {
                                    attr_valid: attr.ttl.as_secs(),
                                    attr_valid_nsec: attr.ttl.subsec_nanos(),
                                    dummy: getattr_in.dummy,
                                    attr: attr.attr.into(),
                                };

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &attr_out)
                                    .expect("won't happened");

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_SETATTR => {
                    let mut resp_sender = self.response_sender.clone();

                    let setattr_in = match BINARY.deserialize::<fuse_setattr_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_setattr_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(setattr_in) => setattr_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        let set_attr = SetAttr::from(&setattr_in);

                        let fh = if setattr_in.valid & FATTR_FH > 0 {
                            Some(setattr_in.fh)
                        } else {
                            None
                        };

                        debug!(
                            "setattr unique {} inode {} set_attr {:?}",
                            request.unique, in_header.nodeid, set_attr
                        );

                        let data = match fs.setattr(request, in_header.nodeid, fh, set_attr).await {
                            Err(err) => {
                                let out_header = fuse_out_header {
                                    len: FUSE_OUT_HEADER_SIZE as u32,
                                    error: err.into(),
                                    unique: request.unique,
                                };

                                BINARY.serialize(&out_header).expect("won't happened")
                            }

                            Ok(attr) => {
                                let attr_out: fuse_attr_out = attr.into();

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &attr_out)
                                    .expect("won't happened");

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_READLINK => {
                    let mut resp_sender = self.response_sender.clone();
                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "readlink unique {} inode {}",
                            request.unique, in_header.nodeid
                        );

                        let data = match fs.readlink(request, in_header.nodeid).await {
                            Err(err) => {
                                let out_header = fuse_out_header {
                                    len: FUSE_OUT_HEADER_SIZE as u32,
                                    error: err.into(),
                                    unique: request.unique,
                                };

                                BINARY.serialize(&out_header).expect("won't happened")
                            }

                            Ok(data) => {
                                let content = data.data.as_ref().as_ref();

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + content.len()) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + content.len());

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");

                                data.extend_from_slice(content);

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_SYMLINK => {
                    let mut resp_sender = self.response_sender.clone();

                    let (name, first_null_index) = match get_first_null_position(data) {
                        None => {
                            error!("symlink has no null, request unique {}", request.unique);

                            reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;

                            continue;
                        }

                        Some(index) => (OsString::from_vec((&data[..index]).to_vec()), index),
                    };

                    data = &data[first_null_index + 1..];

                    let link_name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "symlink has no second null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "symlink unique {} parent {} name {:?} link {:?}",
                            request.unique, in_header.nodeid, name, link_name
                        );

                        let data = match fs
                            .symlink(request, in_header.nodeid, &name, &link_name)
                            .await
                        {
                            Err(err) => {
                                let out_header = fuse_out_header {
                                    len: FUSE_OUT_HEADER_SIZE as u32,
                                    error: err.into(),
                                    unique: request.unique,
                                };

                                BINARY.serialize(&out_header).expect("won't happened")
                            }

                            Ok(entry) => {
                                let entry_out: fuse_entry_out = entry.into();

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &entry_out)
                                    .expect("won't happened");

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_MKNOD => {
                    let mut resp_sender = self.response_sender.clone();

                    let mknod_in = match BINARY.deserialize::<fuse_mknod_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_mknod_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(mknod_in) => mknod_in,
                    };

                    data = &data[FUSE_MKNOD_IN_SIZE..];

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_mknod_in body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "mknod unique {} parent {} name {:?} {:?}",
                            request.unique, in_header.nodeid, name, mknod_in
                        );

                        match fs
                            .mknod(
                                request,
                                in_header.nodeid,
                                &name,
                                mknod_in.mode,
                                mknod_in.rdev,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;
                            }

                            Ok(entry) => {
                                let entry_out: fuse_entry_out = entry.into();

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &entry_out)
                                    .expect("won't happened");

                                let _ = resp_sender.send(data).await;
                            }
                        }
                    });
                }

                fuse_opcode::FUSE_MKDIR => {
                    let mut resp_sender = self.response_sender.clone();

                    let mkdir_in = match BINARY.deserialize::<fuse_mkdir_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_mknod_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(mkdir_in) => mkdir_in,
                    };

                    data = &data[FUSE_MKDIR_IN_SIZE..];

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "deserialize fuse_mknod_in doesn't have null unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "mkdir unique {} parent {} name {:?} {:?}",
                            request.unique, in_header.nodeid, name, mkdir_in
                        );

                        match fs
                            .mkdir(
                                request,
                                in_header.nodeid,
                                &name,
                                mkdir_in.mode,
                                mkdir_in.umask,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;
                            }

                            Ok(entry) => {
                                let entry_out: fuse_entry_out = entry.into();

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &entry_out)
                                    .expect("won't happened");

                                let _ = resp_sender.send(data).await;
                            }
                        }
                    });
                }

                fuse_opcode::FUSE_UNLINK => {
                    let mut resp_sender = self.response_sender.clone();

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "unlink body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "unlink unique {} parent {} name {:?}",
                            request.unique, in_header.nodeid, name
                        );

                        let resp_value =
                            if let Err(err) = fs.unlink(request, in_header.nodeid, &name).await {
                                err.into()
                            } else {
                                0
                            };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_RMDIR => {
                    let mut resp_sender = self.response_sender.clone();

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "rmdir body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "rmdir unique {} parent {} name {:?}",
                            request.unique, in_header.nodeid, name
                        );

                        let resp_value =
                            if let Err(err) = fs.rmdir(request, in_header.nodeid, &name).await {
                                err.into()
                            } else {
                                0
                            };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_RENAME => {
                    let mut resp_sender = self.response_sender.clone();

                    let rename_in = match BINARY.deserialize::<fuse_rename_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_rename_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(rename_in) => rename_in,
                    };

                    data = &data[FUSE_RENAME_IN_SIZE..];

                    let (name, first_null_index) = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_rename_in body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => (OsString::from_vec((&data[..index]).to_vec()), index),
                    };

                    data = &data[first_null_index + 1..];

                    let new_name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_rename_in body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "rename unique {} parent {} name {:?} new parent {} new name {:?}",
                            request.unique, in_header.nodeid, name, rename_in.newdir, new_name
                        );

                        let resp_value = if let Err(err) = fs
                            .rename(
                                request,
                                in_header.nodeid,
                                &name,
                                rename_in.newdir,
                                &new_name,
                            )
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_LINK => {
                    let mut resp_sender = self.response_sender.clone();

                    let link_in = match BINARY.deserialize::<fuse_link_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_link_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(link_in) => link_in,
                    };

                    data = &data[FUSE_LINK_IN_SIZE..];

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_link_in body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "link unique {} inode {} new parent {} new name {:?}",
                            request.unique, link_in.oldnodeid, in_header.nodeid, name
                        );

                        match fs
                            .link(request, link_in.oldnodeid, in_header.nodeid, &name)
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;
                            }

                            Ok(entry) => {
                                let entry_out: fuse_entry_out = entry.into();

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &entry_out)
                                    .expect("won't happened");

                                let _ = resp_sender.send(data).await;
                            }
                        }
                    });
                }

                fuse_opcode::FUSE_OPEN => {
                    let mut resp_sender = self.response_sender.clone();

                    let open_in = match BINARY.deserialize::<fuse_open_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_open_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(open_in) => open_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "open unique {} inode {} flags {}",
                            request.unique, in_header.nodeid, open_in.flags
                        );

                        let opened = match fs.open(request, in_header.nodeid, open_in.flags).await {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(opened) => opened,
                        };

                        let open_out: fuse_open_out = opened.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &open_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_READ => {
                    let mut resp_sender = self.response_sender.clone();

                    let read_in = match BINARY.deserialize::<fuse_read_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_read_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(read_in) => read_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "read unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, read_in
                        );

                        let reply_data = match fs
                            .read(
                                request,
                                in_header.nodeid,
                                read_in.fh,
                                read_in.offset,
                                read_in.size,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_data) => reply_data.data,
                        };

                        let mut reply_data = reply_data.as_ref().as_ref();

                        if reply_data.len() > read_in.size as _ {
                            reply_data = &reply_data[..read_in.size as _];
                        }

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + reply_data.len()) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + reply_data.len());

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");

                        data.extend_from_slice(reply_data);

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_WRITE => {
                    let mut resp_sender = self.response_sender.clone();

                    let write_in = match BINARY.deserialize::<fuse_write_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_write_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(write_in) => write_in,
                    };

                    data = &data[FUSE_WRITE_IN_SIZE..];

                    if write_in.size as usize != data.len() {
                        error!("fuse_write_in body len is invalid");

                        reply_error(libc::EINVAL.into(), request, resp_sender);

                        continue;
                    }

                    let data = data.to_vec();

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "write unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, write_in
                        );

                        let reply_write = match fs
                            .write(
                                request,
                                in_header.nodeid,
                                write_in.fh,
                                write_in.offset,
                                &data,
                                write_in.flags,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_write) => reply_write,
                        };

                        let write_out: fuse_write_out = reply_write.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &write_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_STATFS => {
                    let mut resp_sender = self.response_sender.clone();
                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "statfs unique {} inode {}",
                            request.unique, in_header.nodeid
                        );

                        let fs_stat = match fs.statsfs(request, in_header.nodeid).await {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(fs_stat) => fs_stat,
                        };

                        let statfs_out: fuse_statfs_out = fs_stat.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &statfs_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_RELEASE => {
                    let mut resp_sender = self.response_sender.clone();

                    let release_in = match BINARY.deserialize::<fuse_release_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_release_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(release_in) => release_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        let flush = release_in.release_flags & FUSE_RELEASE_FLUSH > 0;

                        debug!(
                            "release unique {} inode {} fh {} flags {} lock_owner {} flush {}",
                            request.unique,
                            in_header.nodeid,
                            release_in.fh,
                            release_in.flags,
                            release_in.lock_owner,
                            flush
                        );

                        let resp_value = if let Err(err) = fs
                            .release(
                                request,
                                in_header.nodeid,
                                release_in.fh,
                                release_in.flags,
                                release_in.lock_owner,
                                flush,
                            )
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_FSYNC => {
                    let mut resp_sender = self.response_sender.clone();

                    let fsync_in = match BINARY.deserialize::<fuse_fsync_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_fsync_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(fsync_in) => fsync_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        let data_sync = fsync_in.fsync_flags & 1 > 0;

                        debug!(
                            "fsync unique {} inode {} fh {} data_sync {}",
                            request.unique, in_header.nodeid, fsync_in.fh, data_sync
                        );

                        let resp_value = if let Err(err) = fs
                            .fsync(request, in_header.nodeid, fsync_in.fh, data_sync)
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_SETXATTR => {
                    let mut resp_sender = self.response_sender.clone();

                    let setxattr_in = match BINARY.deserialize::<fuse_setxattr_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_setxattr_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(setxattr_in) => setxattr_in,
                    };

                    data = &data[FUSE_SETXATTR_IN_SIZE..];

                    if setxattr_in.size as usize != data.len() {
                        error!(
                            "fuse_setxattr_in body length is not right, request unique {}",
                            request.unique
                        );

                        reply_error(libc::EINVAL.into(), request, resp_sender);

                        continue;
                    }

                    let (name, first_null_index) = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_setxattr_in body has no null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => (OsString::from_vec((&data[..index]).to_vec()), index),
                    };

                    data = &data[first_null_index + 1..];

                    let value = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_setxattr_in value has no second null unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "setxattr unique {} inode {}",
                            request.unique, in_header.nodeid
                        );

                        // TODO handle os X argument
                        let resp_value = if let Err(err) = fs
                            .setxattr(
                                request,
                                in_header.nodeid,
                                &name,
                                &value,
                                setxattr_in.flags,
                                0,
                            )
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_GETXATTR => {
                    let mut resp_sender = self.response_sender.clone();

                    let getxattr_in = match BINARY.deserialize::<fuse_getxattr_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_getxattr_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(getxattr_in) => getxattr_in,
                    };

                    data = &data[FUSE_GETXATTR_IN_SIZE..];

                    let name = match get_first_null_position(data) {
                        None => {
                            error!("fuse_getxattr_in body has no null {}", request.unique);

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "getxattr unique {} inode {}",
                            request.unique, in_header.nodeid
                        );

                        let xattr = match fs
                            .getxattr(request, in_header.nodeid, &name, getxattr_in.size)
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(xattr) => xattr,
                        };

                        let data = match xattr {
                            ReplyXAttr::Size(size) => {
                                let getxattr_out = fuse_getxattr_out { size, padding: 0 };

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_GETXATTR_OUT_SIZE) as u32,
                                    error: libc::ERANGE,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &getxattr_out)
                                    .expect("won't happened");

                                data
                            }

                            ReplyXAttr::Data(xattr_data) => {
                                // TODO check is right way or not
                                // TODO should we check data length or not
                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + xattr_data.len()) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + xattr_data.len());

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");

                                data.extend_from_slice(&xattr_data);

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_LISTXATTR => {
                    let mut resp_sender = self.response_sender.clone();

                    let listxattr_in = match BINARY.deserialize::<fuse_getxattr_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_getxattr_in in listxattr failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(listxattr_in) => listxattr_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "listxattr unique {} inode {} size {}",
                            request.unique, in_header.nodeid, listxattr_in.size
                        );

                        let xattr = match fs
                            .listxattr(request, in_header.nodeid, listxattr_in.size)
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(xattr) => xattr,
                        };

                        let data = match xattr {
                            ReplyXAttr::Size(size) => {
                                let getxattr_out = fuse_getxattr_out { size, padding: 0 };

                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + FUSE_GETXATTR_OUT_SIZE) as u32,
                                    error: libc::ERANGE,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE);

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");
                                BINARY
                                    .serialize_into(&mut data, &getxattr_out)
                                    .expect("won't happened");

                                data
                            }

                            ReplyXAttr::Data(xattr_data) => {
                                // TODO check is right way or not
                                // TODO should we check data length or not
                                let out_header = fuse_out_header {
                                    len: (FUSE_OUT_HEADER_SIZE + xattr_data.len()) as u32,
                                    error: 0,
                                    unique: request.unique,
                                };

                                let mut data =
                                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + xattr_data.len());

                                BINARY
                                    .serialize_into(&mut data, &out_header)
                                    .expect("won't happened");

                                data.extend_from_slice(&xattr_data);

                                data
                            }
                        };

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_REMOVEXATTR => {
                    let mut resp_sender = self.response_sender.clone();

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse removexattr body has no null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "removexattr unique {} inode {}",
                            request.unique, in_header.nodeid
                        );

                        let resp_value = if let Err(err) =
                            fs.removexattr(request, in_header.nodeid, &name).await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_FLUSH => {
                    let mut resp_sender = self.response_sender.clone();

                    let flush_in = match BINARY.deserialize::<fuse_flush_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_flush_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;

                            continue;
                        }

                        Ok(flush_in) => flush_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "flush unique {} inode {} fh {} lock_owner {}",
                            request.unique, in_header.nodeid, flush_in.fh, flush_in.lock_owner
                        );

                        let resp_value = if let Err(err) = fs
                            .flush(request, in_header.nodeid, flush_in.fh, flush_in.lock_owner)
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_OPENDIR => {
                    let mut resp_sender = self.response_sender.clone();

                    let open_in = match BINARY.deserialize::<fuse_open_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_open_in in opendir failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(open_in) => open_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "opendir unique {} inode {} flags {}",
                            request.unique, in_header.nodeid, open_in.flags
                        );

                        let reply_open =
                            match fs.opendir(request, in_header.nodeid, open_in.flags).await {
                                Err(err) => {
                                    reply_error_in_place(err, request, resp_sender).await;

                                    return;
                                }

                                Ok(reply_open) => reply_open,
                            };

                        let open_out: fuse_open_out = reply_open.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &open_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_READDIR => {
                    let mut resp_sender = self.response_sender.clone();

                    if self.mount_options.force_readdir_plus {
                        reply_error(libc::ENOSYS.into(), request, resp_sender);

                        continue;
                    }

                    let read_in = match BINARY.deserialize::<fuse_read_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_read_in in readdir failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(read_in) => read_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "readdir unique {} inode {} fh {} offset {}",
                            request.unique, in_header.nodeid, read_in.fh, read_in.offset
                        );

                        let mut reply_readdir = match fs
                            .readdir(request, in_header.nodeid, read_in.fh, read_in.offset as i64)
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_readdir) => reply_readdir,
                        };

                        let max_size = read_in.size as usize;

                        let mut entry_data = Vec::with_capacity(max_size);

                        while let Some(entry) = reply_readdir.entries.next().await {
                            let name = &entry.name;

                            let dir_entry_size = FUSE_DIRENT_SIZE + name.len();

                            let padding_size = get_padding_size(dir_entry_size);

                            if entry_data.len() + dir_entry_size > max_size {
                                break;
                            }

                            let dir_entry = fuse_dirent {
                                ino: entry.inode,
                                off: entry.index,
                                namelen: name.len() as u32,
                                // learn from fuse-rs and golang bazil.org fuse DirentType
                                r#type: mode_from_kind_and_perm(entry.kind, 0) >> 12,
                            };

                            BINARY
                                .serialize_into(&mut entry_data, &dir_entry)
                                .expect("won't happened");

                            entry_data.extend_from_slice(name.as_bytes());

                            // padding
                            for _ in 0..padding_size {
                                entry_data.push(0);
                            }
                        }

                        // TODO find a way to avoid multi allocate

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + entry_data.len()) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + entry_data.len());

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");

                        data.extend_from_slice(&entry_data);

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_RELEASEDIR => {
                    let mut resp_sender = self.response_sender.clone();

                    let release_in = match BINARY.deserialize::<fuse_release_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_release_in in releasedir failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(release_in) => release_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "releasedir unique {} inode {} fh {} flags {}",
                            request.unique, in_header.nodeid, release_in.fh, release_in.flags
                        );

                        let resp_value = if let Err(err) = fs
                            .releasedir(request, in_header.nodeid, release_in.fh, release_in.flags)
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_FSYNCDIR => {
                    let mut resp_sender = self.response_sender.clone();

                    let fsync_in = match BINARY.deserialize::<fuse_fsync_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_fsync_in in fsyncdir failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(fsync_in) => fsync_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        let data_sync = fsync_in.fsync_flags & 1 > 0;

                        debug!(
                            "fsyncdir unique {} inode {} fh {} data_sync {}",
                            request.unique, in_header.nodeid, fsync_in.fh, data_sync
                        );

                        let resp_value = if let Err(err) = fs
                            .fsyncdir(request, in_header.nodeid, fsync_in.fh, data_sync)
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                #[cfg(feature = "file-lock")]
                fuse_opcode::FUSE_GETLK => {
                    let mut resp_sender = self.response_sender.clone();

                    let getlk_in = match BINARY.deserialize::<fuse_lk_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_lk_in in getlk failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;

                            continue;
                        }

                        Ok(getlk_in) => getlk_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "getlk unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, getlk_in
                        );

                        let reply_lock = match fs
                            .getlk(
                                request,
                                in_header.nodeid,
                                getlk_in.fh,
                                getlk_in.owner,
                                getlk_in.lk.start,
                                getlk_in.lk.end,
                                getlk_in.lk.r#type,
                                getlk_in.lk.pid,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_lock) => reply_lock,
                        };

                        let getlk_out: fuse_lk_out = reply_lock.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_LK_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_LK_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &getlk_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                #[cfg(feature = "file-lock")]
                fuse_opcode::FUSE_SETLK | fuse_opcode::FUSE_SETLKW => {
                    let mut resp_sender = self.response_sender.clone();

                    let setlk_in = match BINARY.deserialize::<fuse_lk_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_lk_in in {:?} failed {}, request unique {}",
                                opcode, err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(setlk_in) => setlk_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        let block = opcode == fuse_opcode::FUSE_SETLKW;

                        debug!(
                            "setlk unique {} inode {} block {} {:?}",
                            request.unique, in_header.nodeid, block, setlk_in
                        );

                        let resp = if let Err(err) = fs
                            .setlk(
                                request,
                                in_header.nodeid,
                                setlk_in.fh,
                                setlk_in.owner,
                                setlk_in.lk.start,
                                setlk_in.lk.end,
                                setlk_in.lk.r#type,
                                setlk_in.lk.pid,
                                block,
                            )
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp,
                            unique: request.unique,
                        };

                        let data = BINARY
                            .serialize(&out_header)
                            .expect("can't serialize into vec");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_ACCESS => {
                    let mut resp_sender = self.response_sender.clone();

                    let access_in = match BINARY.deserialize::<fuse_access_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_access_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(access_in) => access_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "access unique {} inode {} mask {}",
                            request.unique, in_header.nodeid, access_in.mask
                        );

                        let resp_value = if let Err(err) =
                            fs.access(request, in_header.nodeid, access_in.mask).await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        debug!("access response {}", resp_value);

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_CREATE => {
                    let mut resp_sender = self.response_sender.clone();

                    let create_in = match BINARY.deserialize::<fuse_create_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_create_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(create_in) => create_in,
                    };

                    data = &data[FUSE_CREATE_IN_SIZE..];

                    let name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_create_in body has no null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "create unique {} parent {} name {:?} mode {} flags {}",
                            request.unique, in_header.nodeid, name, create_in.mode, create_in.flags
                        );

                        let created = match fs
                            .create(
                                request,
                                in_header.nodeid,
                                &name,
                                create_in.mode,
                                create_in.flags,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(created) => created,
                        };

                        let (entry_out, open_out): (fuse_entry_out, fuse_open_out) = created.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE + FUSE_OPEN_OUT_SIZE)
                                as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data = Vec::with_capacity(
                            FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE + FUSE_OPEN_OUT_SIZE,
                        );

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &entry_out)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &open_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_INTERRUPT => {
                    let mut resp_sender = self.response_sender.clone();

                    let interrupt_in = match BINARY.deserialize::<fuse_interrupt_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_interrupt_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(interrupt_in) => interrupt_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "interrupt_in unique {} interrupt unique {}",
                            request.unique, interrupt_in.unique
                        );

                        let resp_value =
                            if let Err(err) = fs.interrupt(request, interrupt_in.unique).await {
                                err.into()
                            } else {
                                0
                            };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_BMAP => {
                    let mut resp_sender = self.response_sender.clone();

                    let bmap_in = match BINARY.deserialize::<fuse_bmap_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_bmap_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(bmap_in) => bmap_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "bmap unique {} inode {} block size {} idx {}",
                            request.unique, in_header.nodeid, bmap_in.blocksize, bmap_in.block
                        );

                        let reply_bmap = match fs
                            .bmap(request, in_header.nodeid, bmap_in.blocksize, bmap_in.block)
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_bmap) => reply_bmap,
                        };

                        let bmap_out: fuse_bmap_out = reply_bmap.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_BMAP_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_BMAP_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &bmap_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                /*fuse_opcode::FUSE_IOCTL => {
                    let mut resp_sender = self.response_sender.clone();

                    let ioctl_in = match BINARY.deserialize::<fuse_ioctl_in>(data) {
                        Err(err) => {
                            error!("deserialize fuse_ioctl_in failed {}", err);

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(ioctl_in) => ioctl_in,
                    };

                    let ioctl_data = (&data[FUSE_IOCTL_IN_SIZE..]).to_vec();

                    let fs = fs.clone();
                }*/
                fuse_opcode::FUSE_POLL => {
                    let mut resp_sender = self.response_sender.clone();

                    let poll_in = match BINARY.deserialize::<fuse_poll_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_poll_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(poll_in) => poll_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "poll unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, poll_in
                        );

                        let kh = if poll_in.flags & FUSE_POLL_SCHEDULE_NOTIFY > 0 {
                            Some(poll_in.kh)
                        } else {
                            None
                        };

                        let reply_poll = match fs
                            .poll(
                                request,
                                in_header.nodeid,
                                poll_in.fh,
                                kh,
                                poll_in.flags,
                                poll_in.events,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_poll) => reply_poll,
                        };

                        let poll_out: fuse_poll_out = reply_poll.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_POLL_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_POLL_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &poll_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_NOTIFY_REPLY => {
                    let resp_sender = self.response_sender.clone();

                    let notify_retrieve_in =
                        match BINARY.deserialize::<fuse_notify_retrieve_in>(data) {
                            Err(err) => {
                                error!(
                                "deserialize fuse_notify_retrieve_in failed {}, request unique {}",
                                err, request.unique
                            );

                                // TODO need to reply or not?
                                continue;
                            }

                            Ok(notify_retrieve_in) => notify_retrieve_in,
                        };

                    data = &data[FUSE_NOTIFY_RETRIEVE_IN_SIZE..];

                    if data.len() < notify_retrieve_in.size as usize {
                        error!(
                            "fuse_notify_retrieve unique {} data size is not right",
                            request.unique
                        );

                        // TODO need to reply or not?
                        continue;
                    }

                    let data = (&data[..notify_retrieve_in.size as usize]).to_vec();

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        if let Err(err) = fs
                            .notify_reply(
                                request,
                                in_header.nodeid,
                                notify_retrieve_in.offset,
                                data,
                            )
                            .await
                        {
                            reply_error_in_place(err, request, resp_sender).await;
                        }
                    });
                }

                fuse_opcode::FUSE_BATCH_FORGET => {
                    let batch_forget_in = match BINARY.deserialize::<fuse_batch_forget_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_batch_forget_in failed {}, request unique {}",
                                err, request.unique
                            );

                            // no need to reply
                            continue;
                        }

                        Ok(batch_forget_in) => batch_forget_in,
                    };

                    let mut forgets = vec![];

                    data = &data[FUSE_BATCH_FORGET_IN_SIZE..];

                    // TODO if has less data, should I return error?
                    while data.len() >= FUSE_FORGET_ONE_SIZE {
                        match BINARY.deserialize::<fuse_forget_one>(data) {
                            Err(err) => {
                                error!("deserialize fuse_batch_forget_in body fuse_forget_one failed {}, request unique {}", err, request.unique);

                                // no need to reply
                                continue 'dispatch_loop;
                            }

                            Ok(forget_one) => {
                                data = &data[FUSE_FORGET_ONE_SIZE..];

                                forgets.push(forget_one);
                            }
                        }
                    }

                    if forgets.len() != batch_forget_in.count as usize {
                        error!("fuse_forget_one count != fuse_batch_forget_in.count, request unique {}", request.unique);

                        continue;
                    }

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        let inodes = forgets
                            .into_iter()
                            .map(|forget_one| forget_one.nodeid)
                            .collect::<Vec<_>>();

                        debug!("batch_forget unique {} inodes {:?}", request.unique, inodes);

                        fs.batch_forget(request, &inodes).await
                    });
                }

                fuse_opcode::FUSE_FALLOCATE => {
                    let mut resp_sender = self.response_sender.clone();

                    let fallocate_in = match BINARY.deserialize::<fuse_fallocate_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_fallocate_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(fallocate_in) => fallocate_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "fallocate unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, fallocate_in
                        );

                        let resp_value = if let Err(err) = fs
                            .fallocate(
                                request,
                                in_header.nodeid,
                                fallocate_in.fh,
                                fallocate_in.offset,
                                fallocate_in.length,
                                fallocate_in.mode,
                            )
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_READDIRPLUS => {
                    let mut resp_sender = self.response_sender.clone();

                    let readdirplus_in = match BINARY.deserialize::<fuse_read_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_read_in in readdirplus failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(readdirplus_in) => readdirplus_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "readdirplus unique {} parent {} {:?}",
                            request.unique, in_header.nodeid, readdirplus_in
                        );

                        let mut directory_plus = match fs
                            .readdirplus(
                                request,
                                in_header.nodeid,
                                readdirplus_in.fh,
                                readdirplus_in.offset,
                                readdirplus_in.lock_owner,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(directory_plus) => directory_plus,
                        };

                        let max_size = readdirplus_in.size as usize;

                        let mut entry_data = Vec::with_capacity(max_size);

                        while let Some(entry) = directory_plus.entries.next().await {
                            let name = &entry.name;

                            let dir_entry_size = FUSE_DIRENTPLUS_SIZE + name.len();

                            let padding_size = get_padding_size(dir_entry_size);

                            if entry_data.len() + dir_entry_size > max_size {
                                break;
                            }

                            let attr = entry.attr;

                            let dir_entry = fuse_direntplus {
                                entry_out: fuse_entry_out {
                                    nodeid: attr.ino,
                                    generation: entry.generation,
                                    entry_valid: entry.entry_ttl.as_secs(),
                                    attr_valid: entry.attr_ttl.as_secs(),
                                    entry_valid_nsec: entry.entry_ttl.subsec_nanos(),
                                    attr_valid_nsec: entry.attr_ttl.subsec_nanos(),
                                    attr: attr.into(),
                                },
                                dirent: fuse_dirent {
                                    ino: entry.inode,
                                    off: entry.index,
                                    namelen: name.len() as u32,
                                    // learn from fuse-rs and golang bazil.org fuse DirentType
                                    r#type: mode_from_kind_and_perm(entry.kind, 0) >> 12,
                                },
                            };

                            BINARY
                                .serialize_into(&mut entry_data, &dir_entry)
                                .expect("won't happened");

                            entry_data.extend_from_slice(name.as_bytes());

                            // padding
                            for _ in 0..padding_size {
                                entry_data.push(0);
                            }
                        }

                        // TODO find a way to avoid multi allocate

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + entry_data.len()) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + entry_data.len());

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");

                        data.extend_from_slice(&entry_data);

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_RENAME2 => {
                    let mut resp_sender = self.response_sender.clone();

                    let rename2_in = match BINARY.deserialize::<fuse_rename2_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_rename2_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(rename2_in) => rename2_in,
                    };

                    data = &data[FUSE_RENAME2_IN_SIZE..];

                    let (old_name, index) = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_rename2_in body doesn't have null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => (OsString::from_vec((&data[..index]).to_vec()), index),
                    };

                    data = &data[index + 1..];

                    let new_name = match get_first_null_position(data) {
                        None => {
                            error!(
                                "fuse_rename2_in body doesn't have second null, request unique {}",
                                request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Some(index) => OsString::from_vec((&data[..index]).to_vec()),
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!("rename2 unique {} parent {} name {:?} new parent {} new name {:?} flags {}", request.unique, in_header.nodeid, old_name, rename2_in.newdir, new_name, rename2_in.flags);

                        let resp_value = if let Err(err) = fs
                            .rename2(
                                request,
                                in_header.nodeid,
                                &old_name,
                                rename2_in.newdir,
                                &new_name,
                                rename2_in.flags,
                            )
                            .await
                        {
                            err.into()
                        } else {
                            0
                        };

                        let out_header = fuse_out_header {
                            len: FUSE_OUT_HEADER_SIZE as u32,
                            error: resp_value,
                            unique: request.unique,
                        };

                        let data = BINARY.serialize(&out_header).expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_LSEEK => {
                    let mut resp_sender = self.response_sender.clone();

                    let lseek_in = match BINARY.deserialize::<fuse_lseek_in>(data) {
                        Err(err) => {
                            error!(
                                "deserialize fuse_lseek_in failed {}, request unique {}",
                                err, request.unique
                            );

                            reply_error(libc::EINVAL.into(), request, resp_sender);

                            continue;
                        }

                        Ok(lseek_in) => lseek_in,
                    };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "lseek unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, lseek_in
                        );

                        let reply_lseek = match fs
                            .lseek(
                                request,
                                in_header.nodeid,
                                lseek_in.fh,
                                lseek_in.offset,
                                lseek_in.whence,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_lseek) => reply_lseek,
                        };

                        let lseek_out: fuse_lseek_out = reply_lseek.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_LSEEK_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &lseek_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                fuse_opcode::FUSE_COPY_FILE_RANGE => {
                    let mut resp_sender = self.response_sender.clone();

                    let copy_file_range_in =
                        match BINARY.deserialize::<fuse_copy_file_range_in>(data) {
                            Err(err) => {
                                error!(
                                "deserialize fuse_copy_file_range_in failed {}, request unique {}",
                                err, request.unique
                            );

                                reply_error(libc::EINVAL.into(), request, resp_sender);

                                continue;
                            }

                            Ok(copy_file_range_in) => copy_file_range_in,
                        };

                    let fs = fs.clone();

                    spawn_without_return(async move {
                        debug!(
                            "reply_copy_file_range unique {} inode {} {:?}",
                            request.unique, in_header.nodeid, copy_file_range_in
                        );

                        let reply_copy_file_range = match fs
                            .copy_file_range(
                                request,
                                in_header.nodeid,
                                copy_file_range_in.fh_in,
                                copy_file_range_in.off_in,
                                copy_file_range_in.nodeid_out,
                                copy_file_range_in.fh_out,
                                copy_file_range_in.off_out,
                                copy_file_range_in.len,
                                copy_file_range_in.flags,
                            )
                            .await
                        {
                            Err(err) => {
                                reply_error_in_place(err, request, resp_sender).await;

                                return;
                            }

                            Ok(reply_copy_file_range) => reply_copy_file_range,
                        };

                        let write_out: fuse_write_out = reply_copy_file_range.into();

                        let out_header = fuse_out_header {
                            len: (FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE) as u32,
                            error: 0,
                            unique: request.unique,
                        };

                        let mut data =
                            Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE);

                        BINARY
                            .serialize_into(&mut data, &out_header)
                            .expect("won't happened");
                        BINARY
                            .serialize_into(&mut data, &write_out)
                            .expect("won't happened");

                        let _ = resp_sender.send(data).await;
                    });
                }

                #[cfg(target_os = "macos")]
                fuse_opcode::FUSE_SETVOLNAME => {}

                #[cfg(target_os = "macos")]
                fuse_opcode::FUSE_GETXTIMES => {}

                #[cfg(target_os = "macos")]
                fuse_opcode::FUSE_EXCHANGE => {} // fuse_opcode::CUSE_INIT => {}
            }
        }
    }
}

fn reply_error<S>(err: Errno, request: Request, sender: S)
where
    S: Sink<Vec<u8>> + Send + Sync + 'static + Unpin,
{
    spawn_without_return(reply_error_in_place(err, request, sender));
}

async fn reply_error_in_place<S>(err: Errno, request: Request, mut sender: S)
where
    S: Sink<Vec<u8>> + Send + Sync + 'static + Unpin,
{
    let out_header = fuse_out_header {
        len: FUSE_OUT_HEADER_SIZE as u32,
        error: err.into(),
        unique: request.unique,
    };

    let data = BINARY.serialize(&out_header).expect("won't happened");

    let _ = sender.send(data).await;
}
