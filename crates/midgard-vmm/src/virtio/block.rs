//! virtio-blk backed by a disk image (raw or qcow2).
//!
//! When the guest notifies the queue, the device's queue task only takes the requests
//! from it and checks them. Their I/O runs as tasks of their own on the VM's executor
//! (see [`start`]), concurrently; the image, which only those tasks use, moves the data
//! directly between the disk and guest memory (see [`Image`]). Requests then complete
//! through [`Board::with_device`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_executor::{Executor, Task};
use midgard_hv::GuestMemory;
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{Chain, Device, DeviceRef, Error, InFlight, Pending, Queue, read_config};
use crate::board::Board;
use crate::host::image::{Buffer, Image};
use crate::logging::log;

const NAME: &str = "virtio-blk";
const DEVICE_ID: u32 = 2;
const QUEUE_SIZE: u16 = 256;
const SECTOR_SIZE: u64 = 512;

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

/// struct virtio_blk_outhdr: the header of a request.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct RequestHeader {
    kind: U32,
    ioprio: U32,
    sector: U64,
}

/// struct virtio_blk_config (up to blk_size).
#[derive(Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Config {
    capacity: U64,
    size_max: U32,
    seg_max: U32,
    cylinders: U16,
    heads: u8,
    sectors: u8,
    blk_size: U32,
}

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

pub(crate) struct Block {
    capacity: u64,
    read_only: bool,
    id: [u8; 20],
    /// Requests whose I/O the device's task runs (see [`start`]).
    requests: async_channel::Sender<Request>,
    /// The requests that were sent and are not completed (or dropped) yet.
    in_flight: InFlight,
    /// Counts the driver's resets: requests from before a reset do not start their I/O
    /// and are not returned to the driver.
    generation: Arc<AtomicU64>,
}

/// What a request does on the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Read,
    Write,
    Flush,
}

/// The host side of a block device (see [`start`]): its image and the requests whose
/// I/O it runs.
pub(crate) struct Backend {
    image: Arc<dyn Image>,
    requests: async_channel::Receiver<Request>,
    /// See [`Block::generation`].
    generation: Arc<AtomicU64>,
}

/// A request that does I/O.
pub(crate) struct Request {
    /// Counts the request until it is completed (see [`Block::in_flight`]).
    pending: Pending,
    head: u16,
    generation: u64,
    operation: Operation,
    /// Where the I/O starts on the disk.
    offset: u64,
    /// The data buffers, in order.
    buffers: Vec<Buffer>,
    /// The address of the status byte.
    status: u64,
}

/// A request whose I/O is done.
struct Completion {
    /// Ends the request's count when the completion is dropped.
    _pending: Pending,
    head: u16,
    generation: u64,
    status: u64,
    code: u8,
    /// The number of bytes that the I/O wrote into the data buffers.
    written: u32,
}

/// What to do with a request (see [`Block::check`]).
enum Action {
    /// The request is done (without I/O).
    Complete { code: u8, written: u32 },
    Io {
        operation: Operation,
        offset: u64,
        buffers: Vec<Buffer>,
    },
}

impl Block {
    /// The device on `image`, and its host side (for [`start`]).
    pub(crate) fn new(image: Arc<dyn Image>, read_only: bool, index: usize) -> (Self, Backend) {
        let capacity = image.size() / SECTOR_SIZE;
        let mut id = [0u8; 20];
        let name = format!("midgard-disk{index}");
        id[..name.len()].copy_from_slice(name.as_bytes());
        let (requests, receiver) = async_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let block = Self {
            capacity,
            read_only,
            id,
            requests,
            in_flight: InFlight::default(),
            generation: generation.clone(),
        };
        let backend = Backend {
            image,
            requests: receiver,
            generation,
        };
        (block, backend)
    }

    /// Checks a request (all of its buffers, such that a bad request does nothing).
    fn check(&self, chain: &Chain, memory: &GuestMemory) -> Result<Action, Error> {
        let header = chain
            .descriptors
            .first()
            .ok_or(Error::Malformed("empty request"))?;
        if header.writable || (header.length as usize) < size_of::<RequestHeader>() {
            return Err(Error::Malformed("bad request header"));
        }
        let request: RequestHeader = memory
            .read_obj(header.address)
            .map_err(|_| Error::OutsideRam("request header"))?;
        let kind = request.kind.get();
        let sector = request.sector.get();
        // Data buffers are between the header and the status byte.
        let data = &chain.descriptors[1..chain.descriptors.len() - 1];
        let failed = Action::Complete {
            code: S_IOERR,
            written: 0,
        };

        match kind {
            T_IN | T_OUT => {
                if kind == T_OUT && self.read_only {
                    return Ok(failed);
                }
                let total: u64 = data.iter().map(|d| d.length as u64).sum();
                // The used length (with the status byte) must fit into 32 bits.
                if total >= u32::MAX as u64 {
                    return Err(Error::Malformed("request is too large"));
                }
                if sector
                    .checked_add(total.div_ceil(SECTOR_SIZE))
                    .is_none_or(|end| end > self.capacity)
                {
                    return Ok(failed);
                }
                for descriptor in data {
                    if descriptor.writable != (kind == T_IN) {
                        return Err(Error::Malformed("data buffer has the wrong direction"));
                    }
                    memory
                        .check_range(descriptor.address, descriptor.length as usize)
                        .map_err(|_| Error::OutsideRam("data buffer"))?;
                }
                Ok(Action::Io {
                    operation: if kind == T_IN {
                        Operation::Read
                    } else {
                        Operation::Write
                    },
                    offset: sector * SECTOR_SIZE,
                    buffers: data
                        .iter()
                        .map(|d| Buffer {
                            address: d.address,
                            length: d.length as usize,
                        })
                        .collect(),
                })
            }
            T_FLUSH => Ok(Action::Io {
                operation: Operation::Flush,
                offset: 0,
                buffers: Vec::new(),
            }),
            T_GET_ID => {
                let Some(buffer) = data.first().filter(|d| d.writable) else {
                    return Ok(failed);
                };
                let length = (buffer.length as usize).min(self.id.len());
                memory
                    .write(buffer.address, &self.id[..length])
                    .map_err(|_| Error::OutsideRam("ID buffer"))?;
                Ok(Action::Complete {
                    code: S_OK,
                    written: length as u32,
                })
            }
            _ => Ok(Action::Complete {
                code: S_UNSUPP,
                written: 0,
            }),
        }
    }

    /// Returns a request whose I/O is done to the driver (unless the driver reset the
    /// device in the meantime).
    fn complete(&self, completion: Completion, queues: &mut [Queue], memory: &GuestMemory) {
        if completion.generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        finish(
            &mut queues[0],
            memory,
            completion.head,
            completion.status,
            completion.code,
            completion.written,
        );
    }
}

/// Writes the status byte at `status` and returns the request to the driver.
fn finish(queue: &mut Queue, memory: &GuestMemory, head: u16, status: u64, code: u8, written: u32) {
    if memory.write(status, &[code]).is_err() {
        log!("{NAME}: the status byte is outside of RAM");
    }
    queue.push(memory, head, written + 1);
}

impl Request {
    /// Runs the request's I/O on `image`, unless the driver reset the device since it
    /// sent the request (`generation` is the current one).
    async fn execute(
        self,
        image: &dyn Image,
        memory: GuestMemory,
        generation: &AtomicU64,
    ) -> Completion {
        let mut completion = Completion {
            _pending: self.pending,
            head: self.head,
            generation: self.generation,
            status: self.status,
            code: S_IOERR,
            written: 0,
        };
        if self.generation != generation.load(Ordering::SeqCst) {
            return completion;
        }
        let length: usize = self.buffers.iter().map(|buffer| buffer.length).sum();
        let (io, written) = match self.operation {
            Operation::Read => (image.read(memory, self.buffers, self.offset), length),
            Operation::Write => (image.write(memory, self.buffers, self.offset), 0),
            Operation::Flush => (image.flush(), 0),
        };
        match io.await {
            Ok(()) => {
                completion.code = S_OK;
                // check() made sure that this fits.
                completion.written = written as u32;
            }
            // The data buffers' contents are undefined.
            Err(error) => log!(
                "{NAME}: {:?} at offset {:#x}: {error}",
                self.operation,
                self.offset
            ),
        }
        completion
    }
}

/// Lets the VM wait for a block device's I/O when it stops (see [`start`]).
pub(crate) struct Shutdown {
    requests: async_channel::Receiver<Request>,
    task: Task<()>,
}

impl Shutdown {
    /// Waits for the I/O of all requests (the vCPUs must be stopped, such that there are
    /// no new ones).
    pub(crate) async fn wait(self) {
        self.requests.close();
        self.task.await;
    }
}

/// Runs the I/O of the requests of `device` as tasks on `executor` (requests run
/// concurrently); they complete through `board`.
pub(crate) fn start(
    executor: &Arc<Executor<'static>>,
    board: Arc<Board>,
    device: DeviceRef<Block>,
    backend: Backend,
) -> Shutdown {
    let Backend {
        image,
        requests,
        generation,
    } = backend;
    // The executor owns this task, which must not keep it alive.
    let tasks: Weak<Executor<'static>> = Arc::downgrade(executor);
    let receiver = requests.clone();
    let task = executor.spawn(async move {
        let mut running: Vec<Task<()>> = Vec::new();
        while let Ok(request) = receiver.recv().await {
            let Some(executor) = tasks.upgrade() else {
                break;
            };
            running.retain(|task| !task.is_finished());
            let (image, board, generation) = (image.clone(), board.clone(), generation.clone());
            running.push(executor.spawn(async move {
                let memory = board.memory.clone();
                let completion = request.execute(&*image, memory, &generation).await;
                board.with_device(device, |block, queues, memory| {
                    block.complete(completion, queues, memory);
                });
            }));
        }
        // The channel was closed (see Shutdown::wait()).
        for task in running {
            task.await;
        }
    });
    Shutdown { requests, task }
}

impl Device for Block {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        F_SEG_MAX | F_BLK_SIZE | F_FLUSH | if self.read_only { F_RO } else { 0 }
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = Config {
            capacity: self.capacity.into(),
            // The header and the status byte take two descriptors.
            seg_max: (QUEUE_SIZE as u32 - 2).into(),
            blk_size: (SECTOR_SIZE as u32).into(),
            ..Default::default()
        };
        read_config(config.as_bytes(), offset, data);
    }

    fn process(&mut self, _queue: usize, queues: &mut [Queue], memory: &GuestMemory) {
        while let Some(chain) = queues[0].next_chain(memory, NAME) {
            // The status is the last byte of the request.
            let Some(status) = chain
                .descriptors
                .last()
                .filter(|d| d.writable && d.length >= 1)
                .and_then(|d| d.address.checked_add(d.length as u64 - 1))
            else {
                log!("{NAME}: request without a status byte");
                queues[0].push(memory, chain.head, 0);
                continue;
            };
            let (code, written) = match self.check(&chain, memory) {
                Ok(Action::Complete { code, written }) => (code, written),
                Ok(Action::Io {
                    operation,
                    offset,
                    buffers,
                }) => {
                    let request = Request {
                        pending: self.in_flight.start(),
                        head: chain.head,
                        generation: self.generation.load(Ordering::SeqCst),
                        operation,
                        offset,
                        buffers,
                        status,
                    };
                    // The task is only gone when the VM stops.
                    if self.requests.try_send(request).is_ok() {
                        continue;
                    }
                    (S_IOERR, 0)
                }
                Err(error) => {
                    log!("{NAME}: {error}");
                    (S_IOERR, 0)
                }
            };
            finish(&mut queues[0], memory, chain.head, status, code, written);
        }
    }

    fn reset(&mut self) {
        // Requests from before the reset do not start their I/O, and they are not
        // returned to the driver. The reset completes when the I/O that started is done
        // (see in_flight()).
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn in_flight(&self) -> Option<InFlight> {
        Some(self.in_flight.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use super::super::tests::{USED, descriptor, memory, offer, queue, take_pushed};
    use super::super::{DESC_NEXT, DESC_WRITE};
    use super::*;
    use crate::host::image::Io;

    const HEADER: u64 = 0x4000;
    const DATA: u64 = 0x5000;
    const STATUS: u64 = 0x6000;

    /// A disk in memory, whose I/O is done when it starts.
    struct Memory {
        data: Mutex<Vec<u8>>,
        /// The number of reads and writes.
        operations: Arc<AtomicUsize>,
    }

    impl Image for Memory {
        fn size(&self) -> u64 {
            self.data.lock().unwrap().len() as u64
        }

        fn read(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io {
            self.operations.fetch_add(1, Ordering::SeqCst);
            let disk = self.data.lock().unwrap();
            let mut offset = offset as usize;
            for buffer in buffers {
                let data = &disk[offset..offset + buffer.length];
                memory.write(buffer.address, data).unwrap();
                offset += buffer.length;
            }
            Box::pin(std::future::ready(Ok(())))
        }

        fn write(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io {
            self.operations.fetch_add(1, Ordering::SeqCst);
            let mut disk = self.data.lock().unwrap();
            let mut offset = offset as usize;
            for buffer in buffers {
                let data = &mut disk[offset..offset + buffer.length];
                memory.read(buffer.address, data).unwrap();
                offset += buffer.length;
            }
            Box::pin(std::future::ready(Ok(())))
        }

        fn flush(&self) -> Io {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    /// A device on a disk of `sectors` sectors, its queue, and the requests whose I/O
    /// [`start`] would run.
    struct Test {
        memory: GuestMemory,
        queues: [Queue; 1],
        block: Block,
        backend: Backend,
        /// See [`Memory::operations`].
        operations: Arc<AtomicUsize>,
    }

    impl Test {
        fn new(sectors: usize, read_only: bool) -> Self {
            let operations = Arc::new(AtomicUsize::new(0));
            let disk = Memory {
                data: Mutex::new(vec![0; sectors * SECTOR_SIZE as usize]),
                operations: operations.clone(),
            };
            let (block, backend) = Block::new(Arc::new(disk), read_only, 0);
            Self {
                memory: memory(),
                queues: [queue()],
                block,
                backend,
                operations,
            }
        }

        /// Makes a request with one data buffer of `length` bytes (none if 0) available
        /// in `slot` of the available ring, and notifies the device.
        fn submit(&mut self, slot: u16, kind: u32, sector: u64, length: u32) {
            let memory = &self.memory;
            let header = RequestHeader {
                kind: kind.into(),
                ioprio: 0.into(),
                sector: sector.into(),
            };
            memory.write_obj(HEADER, &header).unwrap();
            memory.write(STATUS, &[0xFF]).unwrap();
            // The previous requests are done, so descriptors 0-2 are free again.
            let data_flags = if kind == T_OUT { 0 } else { DESC_WRITE };
            if length > 0 {
                descriptor(memory, 0, HEADER, 16, DESC_NEXT, 1);
                descriptor(memory, 1, DATA, length, DESC_NEXT | data_flags, 2);
            } else {
                descriptor(memory, 0, HEADER, 16, DESC_NEXT, 2);
            }
            descriptor(memory, 2, STATUS, 1, DESC_WRITE, 0);
            offer(memory, slot, 0);
            self.block.process(0, &mut self.queues, memory);
        }

        /// Runs the I/O of the next request; returns its completion.
        fn execute(&self) -> Completion {
            let request = self.backend.requests.try_recv().unwrap();
            let (image, generation) = (&*self.backend.image, &self.backend.generation);
            async_io::block_on(request.execute(image, self.memory.clone(), generation))
        }

        /// Completes a request; returns whether it was returned to the driver.
        fn complete(&mut self, completion: Completion) -> bool {
            take_pushed(&mut self.queues[0]);
            self.block
                .complete(completion, &mut self.queues, &self.memory);
            take_pushed(&mut self.queues[0])
        }

        /// Submits a request and completes it; returns the status and the length in the
        /// used ring.
        fn request(&mut self, slot: u16, kind: u32, sector: u64, length: u32) -> (u8, u32) {
            self.submit(slot, kind, sector, length);
            if !self.backend.requests.is_empty() {
                let completion = self.execute();
                assert!(self.complete(completion));
            }
            assert_eq!(self.used_index(), slot + 1);
            (self.status(), self.used_length(slot))
        }

        fn status(&self) -> u8 {
            let mut status = [0u8];
            self.memory.read(STATUS, &mut status).unwrap();
            status[0]
        }

        fn used_index(&self) -> u16 {
            self.memory.read_volatile::<u16>(USED + 2).unwrap()
        }

        fn used_length(&self, slot: u16) -> u32 {
            let entry = USED + 4 + slot as u64 * 8;
            self.memory.read_volatile::<u32>(entry + 4).unwrap()
        }
    }

    #[test]
    fn requests() {
        let mut test = Test::new(4, false);
        let mut config = [0u8; 8];
        test.block.read_config(0, &mut config);
        assert_eq!(u64::from_le_bytes(config), 4);

        // Write the last sector and read it back.
        test.memory.write(DATA, &[0xAB; 512]).unwrap();
        assert_eq!(test.request(0, T_OUT, 3, 512), (S_OK, 1));
        test.memory.write(DATA, &[0; 512]).unwrap();
        assert_eq!(test.request(1, T_IN, 3, 512), (S_OK, 513));
        let mut data = [0u8; 512];
        test.memory.read(DATA, &mut data).unwrap();
        assert_eq!(data, [0xAB; 512]);

        // Past the end of the disk.
        assert_eq!(test.request(2, T_IN, 4, 512), (S_IOERR, 1));
        assert_eq!(test.request(3, T_FLUSH, 0, 0), (S_OK, 1));
        assert_eq!(test.request(4, T_GET_ID, 0, 20), (S_OK, 21));
        let mut id = [0u8; 20];
        test.memory.read(DATA, &mut id).unwrap();
        assert!(id.starts_with(b"midgard-disk0\0"));
        assert_eq!(test.request(5, 99, 0, 0), (S_UNSUPP, 1));
    }

    #[test]
    fn requests_complete_later() {
        let mut test = Test::new(4, false);
        test.submit(0, T_IN, 0, 512);
        test.submit(1, T_FLUSH, 0, 0);
        // Nothing is done before the I/O completes, which may be in any order.
        assert_eq!(test.used_index(), 0);
        let read = test.execute();
        let flush = test.execute();
        assert!(test.complete(flush));
        assert!(test.complete(read));
        assert_eq!(test.used_index(), 2);
        assert_eq!(test.used_length(0), 1);
        assert_eq!(test.used_length(1), 513);
    }

    #[test]
    fn reset_drops_requests_in_flight() {
        let mut test = Test::new(4, false);
        // One request's I/O started before the reset, the other one's did not.
        test.submit(0, T_IN, 0, 512);
        test.submit(1, T_IN, 0, 512);
        assert_eq!(test.block.in_flight.count(), 2);
        let started = test.execute();
        test.block.reset();
        let stale = test.execute();
        assert_eq!(test.operations.load(Ordering::SeqCst), 1);
        assert!(!test.complete(started));
        assert!(!test.complete(stale));
        assert_eq!(test.used_index(), 0);
        assert_eq!(test.block.in_flight.count(), 0);
    }

    #[test]
    fn bad_requests_do_nothing() {
        let mut test = Test::new(4, false);
        let memory = &test.memory;
        // A write whose second data buffer is outside of RAM; the status descriptor is
        // longer than one byte.
        let header = RequestHeader {
            kind: T_OUT.into(),
            ioprio: 0.into(),
            sector: 0.into(),
        };
        memory.write_obj(HEADER, &header).unwrap();
        memory.write(DATA, &[0xAB; 512]).unwrap();
        descriptor(memory, 0, HEADER, 16, DESC_NEXT, 1);
        descriptor(memory, 1, DATA, 512, DESC_NEXT, 2);
        descriptor(memory, 2, 1 << 40, 512, DESC_NEXT, 3);
        descriptor(memory, 3, STATUS, 4, DESC_WRITE, 0);
        offer(memory, 0, 0);
        test.block.process(0, &mut test.queues, memory);
        assert!(take_pushed(&mut test.queues[0]));
        assert!(test.backend.requests.is_empty());
        let mut status = [0u8; 4];
        memory.read(STATUS, &mut status).unwrap();
        assert_eq!(status, [0, 0, 0, S_IOERR]);
        assert_eq!(test.request(1, T_IN, 0, 512), (S_OK, 513));
        let mut data = [0xFFu8; 512];
        test.memory.read(DATA, &mut data).unwrap();
        assert_eq!(data, [0; 512]);
    }

    #[test]
    fn read_only() {
        let mut test = Test::new(1, true);
        assert_ne!(test.block.features() & F_RO, 0);
        test.memory.write(DATA, &[0xAB; 512]).unwrap();
        assert_eq!(test.request(0, T_OUT, 0, 512), (S_IOERR, 1));
        assert_eq!(test.request(1, T_IN, 0, 512), (S_OK, 513));
        let mut data = [0xFFu8; 512];
        test.memory.read(DATA, &mut data).unwrap();
        assert_eq!(data, [0; 512]);
    }
}
