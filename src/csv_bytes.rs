use crossbeam_channel::bounded;
use memmap2::Mmap;
use std::fs::File;
use std::io::Read;
use std::mem::MaybeUninit;
use std::ops::Range;
use std::sync::Arc;
use std::thread;

use crate::finite_automata::FiniteAutomata;
use crate::finite_automata::{self, FindARecordEndError};

pub trait AsBytes {
    fn as_bytes(&self) -> &[u8];
}

pub struct WorkChunk<B: AsRef<[u8]> + ?Sized> {
    pub buffer: Arc<B>,
    pub range: Range<usize>,
}

impl<B: AsRef<[u8]> + ?Sized> AsBytes for WorkChunk<B> {
    fn as_bytes(&self) -> &[u8] {
        &self.buffer.as_ref().as_ref()[self.range.clone()]
    }
}

pub trait DataSource {
    type WorkType: AsBytes + Send;
    fn fill_queue(
        &mut self,
        ch_source: &crossbeam_channel::Sender<Self::WorkType>,
        finite_automaton: finite_automata::Nfa,
    ) -> Result<(), FillQueueError>;
}

fn file_read_bytes_to_spare_capacity(file: &mut File, buffer: &mut Vec<u8>) {
    // NOTE: This function will use `std::io::ReadBuf` once stable.

    let original_size = buffer.len();

    // The remaining unused capacity of the Vec
    let spare: &mut [MaybeUninit<u8>] = buffer.spare_capacity_mut();

    // SAFETY:
    // 1. `from_raw_parts_mut` creates a `&mut [u8]` from uninitialized memory. Normally `&mut [u8]` must point to fully
    //    initialized memory, but creating a slice to uninitialized memory is allowed in unsafe code as long as no
    //    element is read before it is initialized.
    // 2. We immediately pass the slice to `Read::read`, which writes up to `spare.len()` bytes.
    // 3. We do not read from the slice before it is initialized, preserving the invariant.
    // 4. Casting from `MaybeUninit<u8>` to `u8` is valid because any bit pattern is valid for `u8`.
    // 5. After the read, we call `set_len(bytes_read + original_size)` to mark exactly the initialized portion of the
    //    buffer as valid, ensuring the Vec is in a safe state.

    let buffer_slice =
        unsafe { std::slice::from_raw_parts_mut(spare.as_ptr() as *mut u8, spare.len()) };

    let bytes_read = file.read(buffer_slice).unwrap();

    // SAFETY: update the Vec's length to include only the initialized bytes + the original size.
    unsafe {
        buffer.set_len(bytes_read + original_size);
    }
}

#[derive(Copy, Clone, Debug)]
pub enum FillQueueError {
    Failed,
}

impl DataSource for File {
    type WorkType = WorkChunk<[u8]>;

    fn fill_queue(
        &mut self,
        ch_source: &crossbeam_channel::Sender<Self::WorkType>,
        finite_automata: finite_automata::Nfa,
    ) -> Result<(), FillQueueError> {
        #[allow(unused_assignments)]
        let mut shared_buffer: Arc<[u8]> = Arc::new([]);
        let mut leftover: &[u8] = &[];
        loop {
            const SIZE_1_MIB: usize = 1024 * 1024;
            const SIZE_128_KIB: usize = 1024 * 128;
            const SIZE_4_KIB: usize = 1024 * 4;

            // Work chunks will be around 128 KiB. We will start scanning for a recordEnd about 4 KiB before the 128
            // KiB boundary. This will leave 48 KiB for scanning to find the recordEnd's (4KiB per work chunk). As long
            // as the average record length is < 4 KiB, the remaining overflow that needs to be copied will be less than
            // 32 KiB.

            let mut buffer: Vec<u8> = Vec::with_capacity(SIZE_1_MIB);
            buffer.extend_from_slice(leftover);
            file_read_bytes_to_spare_capacity(self, &mut buffer);
            if buffer.len() == leftover.len() {
                println!("Producer: DONE");
                break;
            }
            dbg!(leftover.len(), buffer.len());

            shared_buffer = Arc::from(buffer.into_boxed_slice());
            leftover = &shared_buffer;

            let mut cur_offset = 0;
            while (shared_buffer.len() - cur_offset) >= SIZE_128_KIB {
                let search_start_offset = cur_offset + SIZE_128_KIB - SIZE_4_KIB;
                match finite_automata.find_a_record_end(&shared_buffer[search_start_offset..]) {
                    Ok(finite_automata::Offset(end_offset)) => {
                        let work = WorkChunk::<[u8]> {
                            buffer: shared_buffer.clone(),
                            range: cur_offset..search_start_offset + end_offset + 1,
                        };
                        // dbg!(cur_offset, end_offset, shared_buffer.len());
                        println!(
                            "Producer: new job (size: {:?})",
                            (search_start_offset + end_offset) - cur_offset + 1
                        );
                        ch_source.send(work).expect("Should not be disconnected");
                        cur_offset = search_start_offset + end_offset + 1;
                        leftover = &shared_buffer[cur_offset..];
                    }
                    Err(FindARecordEndError::ExceededMaxTries) => {
                        return Err(FillQueueError::Failed);
                    }
                    Err(FindARecordEndError::InputExhausted) => {
                        leftover = &shared_buffer[cur_offset..];
                        break;
                    }
                }
            }
        }
        println!("Producer: Process leftover, size={:?}", leftover.len());
        Ok(())
    }
}

impl DataSource for Mmap {
    type WorkType = WorkChunk<Mmap>;

    fn fill_queue<'a>(
        &mut self,
        _ch_source: &crossbeam_channel::Sender<Self::WorkType>,
        _finite_automaton: finite_automata::Nfa,
    ) -> Result<(), FillQueueError> {
        Ok(())
    }
}

fn run_consumer<T: DataSource + Send>(
    id: usize,
    sink: &crossbeam_channel::Receiver<<T as DataSource>::WorkType>,
    _finite_automaton: finite_automata::Nfa,
) {
    loop {
        match sink.recv() {
            Ok(work) => {
                let size = work.as_bytes().len();
                println!("Consumer_{id}: job size: {size:?}");
            }
            Err(_) => {
                println!("Consumer_{id}: DONE");
                break;
            }
        }
    }
}

/// todo...
/// # Panics
///
/// Will panic if
pub fn process<T: DataSource + Send>(data_source: T) {
    let available_parallelism = std::thread::available_parallelism().expect("known").get();

    let (ch_source, ref ch_sink) = bounded(available_parallelism * 2);

    thread::scope(|s| {
        let available_parallelism = std::thread::available_parallelism().expect("known").get();
        let mut handles = Vec::with_capacity(available_parallelism + 1);

        let finite_automaton = finite_automata::Nfa::default();
        handles.push(s.spawn(move || {
            let mut data_source = data_source;
            data_source
                .fill_queue(&ch_source, finite_automaton)
                .expect("Failed");
        }));

        for id in 0..available_parallelism {
            let finite_automaton = finite_automata::Nfa::default();
            handles.push(s.spawn(move || {
                run_consumer::<T>(id, ch_sink, finite_automaton);
            }));
        }

        for h in handles {
            if let Err(e) = h.join() {
                eprintln!("Thread panicked!");
                std::panic::resume_unwind(e);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    // use super::*;

    use std::fs::File;
    use std::{path::PathBuf, str::FromStr};

    // use crate::csv_bytes::CsvBytes;
    use crate::process;

    #[test]
    fn test_file() {
        process(File::open(PathBuf::from_str("/Users/mike/test.csv").unwrap()).unwrap());
        // let mut csv_bytes =
        //     CsvBytes::from_path(&PathBuf::from_str("/Users/mike/test.csv").unwrap()).unwrap();
        // csv_bytes.process();

        // let mut csv_bytes2 =
        //     CsvBytes::from_path_mmap(&PathBuf::from_str("/Users/mike/test.csv").unwrap()).unwrap();
        // csv_bytes.process();
    }

    #[test]
    fn test_mmap() {
        let file = File::open(PathBuf::from_str("/Users/mike/test.csv").unwrap()).unwrap();

        // Safety: Mmap::map is unsafe because the OS might change the file while mapped
        let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
        process(mmap);
    }
}
