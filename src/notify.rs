//! notify kernel.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use futures_channel::mpsc::UnboundedSender;
use futures_util::sink::SinkExt;

use lazy_static::lazy_static;

use crate::abi::{
    fuse_notify_code, fuse_notify_delete_out, fuse_notify_inval_entry_out,
    fuse_notify_inval_inode_out, fuse_notify_poll_wakeup_out, fuse_notify_retrieve_out,
    fuse_notify_store_out, fuse_out_header, FUSE_NOTIFY_DELETE_OUT_SIZE,
    FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE, FUSE_NOTIFY_INVAL_INODE_OUT_SIZE,
    FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE, FUSE_NOTIFY_RETRIEVE_OUT_SIZE, FUSE_NOTIFY_STORE_OUT_SIZE,
    FUSE_OUT_HEADER_SIZE,
};

lazy_static! {
    static ref BINARY: bincode::Config = {
        let mut cfg = bincode::config();
        cfg.little_endian();

        cfg
    };
}

#[derive(Debug, Clone)]
/// notify kernel there are something need to handle.
pub struct Notify {
    sender: UnboundedSender<Vec<u8>>,
}

impl Notify {
    pub(crate) fn new(sender: UnboundedSender<Vec<u8>>) -> Self {
        Self { sender }
    }

    /// notify kernel there are something need to handle. If notify failed, the `kind` will be
    /// return in `Err`.
    pub async fn notify(&mut self, kind: NotifyKind) -> std::result::Result<(), NotifyKind> {
        let data = match &kind {
            NotifyKind::Wakeup { kh } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_POLL as i32,
                    unique: 0,
                };

                let wakeup_out = fuse_notify_poll_wakeup_out { kh: *kh };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE);

                BINARY
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                BINARY
                    .serialize_into(&mut data, &wakeup_out)
                    .expect("vec size is not enough");

                data
            }

            NotifyKind::InvalidInode { inode, offset, len } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_INVAL_INODE as i32,
                    unique: 0,
                };

                let invalid_inode_out = fuse_notify_inval_inode_out {
                    ino: *inode,
                    off: *offset,
                    len: *len,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE);

                BINARY
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                BINARY
                    .serialize_into(&mut data, &invalid_inode_out)
                    .expect("vec size is not enough");

                data
            }

            NotifyKind::InvalidEntry { parent, name } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY as i32,
                    unique: 0,
                };

                let invalid_entry_out = fuse_notify_inval_entry_out {
                    parent: *parent,
                    namelen: name.len() as _,
                    padding: 0,
                };

                let mut data = Vec::with_capacity(
                    FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE + name.len(),
                );

                BINARY
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                BINARY
                    .serialize_into(&mut data, &invalid_entry_out)
                    .expect("vec size is not enough");

                data.extend_from_slice(name.as_bytes());

                // TODO should I add null at the end?

                data
            }

            NotifyKind::Delete {
                parent,
                child,
                name,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_DELETE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_DELETE as i32,
                    unique: 0,
                };

                let delete_out = fuse_notify_delete_out {
                    parent: *parent,
                    child: *child,
                    namelen: name.len() as _,
                    padding: 0,
                };

                let mut data = Vec::with_capacity(
                    FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_DELETE_OUT_SIZE + name.len(),
                );

                BINARY
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                BINARY
                    .serialize_into(&mut data, &delete_out)
                    .expect("vec size is not enough");

                data.extend_from_slice(name.as_bytes());

                // TODO should I add null at the end?

                data
            }

            NotifyKind::Store {
                inode,
                offset,
                data,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_STORE as i32,
                    unique: 0,
                };

                let store_out = fuse_notify_store_out {
                    nodeid: *inode,
                    offset: *offset,
                    size: data.len() as _,
                    padding: 0,
                };

                let mut data_buf = Vec::with_capacity(
                    FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE + data.len(),
                );

                BINARY
                    .serialize_into(&mut data_buf, &out_header)
                    .expect("vec size is not enough");
                BINARY
                    .serialize_into(&mut data_buf, &store_out)
                    .expect("vec size is not enough");

                data_buf.extend_from_slice(data);

                data_buf
            }

            NotifyKind::Retrieve {
                notify_unique,
                inode,
                offset,
                size,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_RETRIEVE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_RETRIEVE as i32,
                    unique: 0,
                };

                let retrieve_out = fuse_notify_retrieve_out {
                    notify_unique: *notify_unique,
                    nodeid: *inode,
                    offset: *offset,
                    size: *size,
                    padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_RETRIEVE_OUT_SIZE);

                BINARY
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                BINARY
                    .serialize_into(&mut data, &retrieve_out)
                    .expect("vec size is not enough");

                data
            }
        };

        self.sender.send(data).await.or(Err(kind))
    }
}

#[derive(Debug)]
/// the kind of notify.
pub enum NotifyKind {
    /// notify the IO is ready.
    Wakeup { kh: u64 },

    // TODO need check is right or not
    /// notify the cache invalidation about an inode.
    InvalidInode { inode: u64, offset: i64, len: i64 },

    /// notify the invalidation about a directory entry.
    InvalidEntry { parent: u64, name: OsString },

    /// notify a directory entry has been deleted.
    Delete {
        parent: u64,
        child: u64,
        name: OsString,
    },

    /// push the data in an inode for updating the kernel cache.
    Store {
        inode: u64,
        offset: u64,
        data: Vec<u8>,
    },

    /// retrieve data in an inode from the kernel cache.
    Retrieve {
        notify_unique: u64,
        inode: u64,
        offset: u64,
        size: u32,
    },
}
