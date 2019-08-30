// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::vu_common_ctrl::setup_vhost_user;
use super::{Error, Result};
use crate::{
    ActivateError, ActivateResult, Queue, VirtioDevice, VirtioDeviceType, VirtioInterrupt,
    VirtioInterruptType, VirtioSharedMemoryList, VIRTIO_F_VERSION_1,
};
use epoll;
use libc::{self, EFD_NONBLOCK};
use std::cmp;
use std::io;
use std::io::Write;
use std::os::unix::io::{AsRawFd, RawFd};
use std::result;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use vhost_rs::vhost_user::message::{
    VhostUserFSSlaveMsg, VhostUserProtocolFeatures, VhostUserVirtioFeatures,
};
use vhost_rs::vhost_user::{
    HandlerResult, Master, MasterReqHandler, VhostUserMaster, VhostUserMasterReqHandler,
};
use vhost_rs::VhostBackend;
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

const CONFIG_SPACE_TAG_SIZE: usize = 36;
const CONFIG_SPACE_NUM_QUEUES_SIZE: usize = 4;
const CONFIG_SPACE_SIZE: usize = CONFIG_SPACE_TAG_SIZE + CONFIG_SPACE_NUM_QUEUES_SIZE;
const NUM_QUEUE_OFFSET: usize = 1;

struct SlaveReqHandler {
    cache_size: u64,
    mmap_cache_addr: u64,
}

impl VhostUserMasterReqHandler for SlaveReqHandler {
    fn handle_config_change(&mut self) -> HandlerResult<()> {
        debug!("handle_config_change");
        Ok(())
    }

    fn fs_slave_map(&mut self, fs: &VhostUserFSSlaveMsg, fd: RawFd) -> HandlerResult<()> {
        debug!("fs_slave_map");

        let addr = self.mmap_cache_addr + fs.cache_offset[0];
        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                fs.len[0] as usize,
                fs.flags[0].bits() as i32,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                fs.fd_offset[0] as libc::off_t,
            )
        };
        if ret == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let ret = unsafe { libc::close(fd) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn fs_slave_unmap(&mut self, fs: &VhostUserFSSlaveMsg) -> HandlerResult<()> {
        debug!("fs_slave_unmap");

        let mut len = fs.len[0];
        // Need to handle a special case where the slave ask for the unmapping
        // of the entire mapping.
        if len == 0xffff_ffff_ffff_ffff {
            len = self.cache_size;
        }

        let addr = self.mmap_cache_addr + fs.cache_offset[0];
        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                libc::PROT_NONE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                -1,
                0 as libc::off_t,
            )
        };
        if ret == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn fs_slave_sync(&mut self, fs: &VhostUserFSSlaveMsg) -> HandlerResult<()> {
        debug!("fs_slave_sync");

        let addr = self.mmap_cache_addr + fs.cache_offset[0];
        let ret =
            unsafe { libc::msync(addr as *mut libc::c_void, fs.len[0] as usize, libc::MS_SYNC) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }
}

struct FsEpollHandler<S: VhostUserMasterReqHandler> {
    vu_call_evt_queue_list: Vec<(EventFd, Queue)>,
    interrupt_cb: Arc<VirtioInterrupt>,
    kill_evt: EventFd,
    slave_req_handler: Option<MasterReqHandler<S>>,
}

impl<S: VhostUserMasterReqHandler> FsEpollHandler<S> {
    fn run(&mut self) -> result::Result<(), Error> {
        // Create the epoll file descriptor
        let epoll_fd = epoll::create(true).map_err(Error::EpollCreateFd)?;

        for (evt_index, vu_call_evt_queue) in self.vu_call_evt_queue_list.iter().enumerate() {
            // Add events
            epoll::ctl(
                epoll_fd,
                epoll::ControlOptions::EPOLL_CTL_ADD,
                vu_call_evt_queue.0.as_raw_fd(),
                epoll::Event::new(epoll::Events::EPOLLIN, evt_index as u64),
            )
            .map_err(Error::EpollCtl)?;
        }

        let kill_evt_index = self.vu_call_evt_queue_list.len();
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.kill_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, kill_evt_index as u64),
        )
        .map_err(Error::EpollCtl)?;

        let slave_evt_index = if let Some(self_req_handler) = &self.slave_req_handler {
            let index = kill_evt_index + 1;
            epoll::ctl(
                epoll_fd,
                epoll::ControlOptions::EPOLL_CTL_ADD,
                self_req_handler.as_raw_fd(),
                epoll::Event::new(epoll::Events::EPOLLIN, index as u64),
            )
            .map_err(Error::EpollCtl)?;

            Some(index)
        } else {
            None
        };

        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        'epoll: loop {
            let num_events = match epoll::wait(epoll_fd, -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(Error::EpollWait(e));
                }
            };

            for event in events.iter().take(num_events) {
                let ev_type = event.data as usize;

                match ev_type {
                    x if (x < kill_evt_index) => {
                        if let Err(e) = self.vu_call_evt_queue_list[x].0.read() {
                            error!("Failed to get queue event: {:?}", e);
                            break 'epoll;
                        } else if let Err(e) = (self.interrupt_cb)(
                            &VirtioInterruptType::Queue,
                            Some(&self.vu_call_evt_queue_list[x].1),
                        ) {
                            error!(
                                "Failed to signal used queue: {:?}",
                                Error::FailedSignalingUsedQueue(e)
                            );
                            break 'epoll;
                        }
                    }
                    x if (x == kill_evt_index) => {
                        debug!("KILL_EVENT received, stopping epoll loop");
                        break 'epoll;
                    }
                    x if (slave_evt_index.is_some() && slave_evt_index.unwrap() == x) => {
                        if let Some(slave_req_handler) = self.slave_req_handler.as_mut() {
                            slave_req_handler
                                .handle_request()
                                .map_err(Error::VhostUserSlaveRequest)?;
                        }
                    }
                    _ => {
                        error!("Unknown event for virtio-fs");
                    }
                }
            }
        }

        Ok(())
    }
}

pub struct Fs {
    vu: Master,
    queue_sizes: Vec<u16>,
    avail_features: u64,
    acked_features: u64,
    config_space: Vec<u8>,
    kill_evt: Option<EventFd>,
    cache: Option<(VirtioSharedMemoryList, u64)>,
    slave_req_support: bool,
}

impl Fs {
    /// Create a new virtio-fs device.
    pub fn new(
        path: &str,
        tag: &str,
        req_num_queues: usize,
        queue_size: u16,
        cache: Option<(VirtioSharedMemoryList, u64)>,
    ) -> Result<Fs> {
        let mut slave_req_support = false;

        // Calculate the actual number of queues needed.
        let num_queues = NUM_QUEUE_OFFSET + req_num_queues;

        // Connect to the vhost-user socket.
        let mut master =
            Master::connect(path, num_queues as u64).map_err(Error::VhostUserCreateMaster)?;

        // Filling device and vring features VMM supports.
        let mut avail_features =
            1 << VIRTIO_F_VERSION_1 | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

        // Set vhost-user owner.
        master.set_owner().map_err(Error::VhostUserSetOwner)?;

        // Get features from backend, do negotiation to get a feature collection which
        // both VMM and backend support.
        let backend_features = master.get_features().map_err(Error::VhostUserGetFeatures)?;
        avail_features &= backend_features;
        // Set features back is required by the vhost crate mechanism, since the
        // later vhost call will check if features is filled in master before execution.
        master
            .set_features(avail_features)
            .map_err(Error::VhostUserSetFeatures)?;

        // Identify if protocol features are supported by the slave.
        let mut acked_features = 0;
        if avail_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0 {
            acked_features |= VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

            let mut protocol_features = master
                .get_protocol_features()
                .map_err(Error::VhostUserGetProtocolFeatures)?;

            if cache.is_some() {
                protocol_features &= VhostUserProtocolFeatures::MQ
                    | VhostUserProtocolFeatures::REPLY_ACK
                    | VhostUserProtocolFeatures::SLAVE_REQ
                    | VhostUserProtocolFeatures::SLAVE_SEND_FD;
            } else {
                protocol_features &=
                    VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::REPLY_ACK;
            }

            master
                .set_protocol_features(protocol_features)
                .map_err(Error::VhostUserSetProtocolFeatures)?;

            slave_req_support = true;
        }

        // Create virtio device config space.
        // First by adding the tag.
        let mut config_space = tag.to_string().into_bytes();
        config_space.resize(CONFIG_SPACE_SIZE, 0);

        // And then by copying the number of queues.
        let num_queues_slice = (req_num_queues as u32).to_le_bytes();
        config_space[CONFIG_SPACE_TAG_SIZE..CONFIG_SPACE_SIZE].copy_from_slice(&num_queues_slice);

        Ok(Fs {
            vu: master,
            queue_sizes: vec![queue_size; num_queues],
            avail_features,
            acked_features,
            config_space,
            kill_evt: None,
            cache,
            slave_req_support,
        })
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        VirtioDeviceType::TYPE_FS as u32
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes.as_slice()
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => self.avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!("fs: Received request for unknown features page: {}", page);
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => {
                warn!("fs: Cannot acknowledge unknown features page: {}", page);
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("fs: virtio-fs got unknown feature ack: {:x}", v);

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_len = self.config_space.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&self.config_space[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let data_len = data.len() as u64;
        let config_len = self.config_space.len() as u64;
        if offset + data_len > config_len {
            error!("Failed to write config space");
            return;
        }
        let (_, right) = self.config_space.split_at_mut(offset as usize);
        right.copy_from_slice(&data[..]);
    }

    fn activate(
        &mut self,
        mem: Arc<RwLock<GuestMemoryMmap>>,
        interrupt_cb: Arc<VirtioInterrupt>,
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != self.queue_sizes.len() || queue_evts.len() != self.queue_sizes.len() {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                self.queue_sizes.len(),
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let (self_kill_evt, kill_evt) =
            match EventFd::new(EFD_NONBLOCK).and_then(|e| Ok((e.try_clone()?, e))) {
                Ok(v) => v,
                Err(e) => {
                    error!("failed creating kill EventFd pair: {}", e);
                    return Err(ActivateError::BadActivate);
                }
            };
        self.kill_evt = Some(self_kill_evt);

        let vu_call_evt_queue_list = setup_vhost_user(
            &mut self.vu,
            &mem.read().unwrap(),
            queues,
            queue_evts,
            self.acked_features,
        )
        .map_err(ActivateError::VhostUserSetup)?;

        // Initialize slave communication.
        let slave_req_handler = if self.slave_req_support {
            if let Some(cache) = self.cache.clone() {
                let vu_master_req_handler = Arc::new(Mutex::new(SlaveReqHandler {
                    cache_size: cache.0.len,
                    mmap_cache_addr: cache.1,
                }));

                let req_handler = MasterReqHandler::new(vu_master_req_handler).map_err(|e| {
                    ActivateError::VhostUserSetup(Error::MasterReqHandlerCreation(e))
                })?;
                self.vu
                    .set_slave_request_fd(req_handler.get_tx_raw_fd())
                    .map_err(|e| {
                        ActivateError::VhostUserSetup(Error::VhostUserSetSlaveRequestFd(e))
                    })?;
                Some(req_handler)
            } else {
                None
            }
        } else {
            None
        };

        let mut handler = FsEpollHandler {
            vu_call_evt_queue_list,
            interrupt_cb,
            kill_evt,
            slave_req_handler,
        };

        let worker_result = thread::Builder::new()
            .name("virtio_fs".to_string())
            .spawn(move || handler.run());

        if let Err(e) = worker_result {
            error!("failed to spawn virtio_blk worker: {}", e);
            return Err(ActivateError::BadActivate);
        }

        Ok(())
    }

    fn get_shm_regions(&self) -> Option<VirtioSharedMemoryList> {
        if let Some(cache) = self.cache.clone() {
            Some(cache.0)
        } else {
            None
        }
    }
}