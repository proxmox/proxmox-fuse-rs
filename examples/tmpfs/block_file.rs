//! Implements "block" based sparse vector.

use std::io;

pub struct Extent {
    offset: u64,
    data: Vec<u8>,
}

enum SearchBias {
    Low,
    High,
}

pub struct BlockFile {
    extents: Vec<Extent>,
    block_size: usize,
    block_mask: u64,
    max_extent_size: usize,
}

impl BlockFile {
    /// Panics if `block_size` is not a power of two.
    pub fn new(block_size: usize) -> Self {
        if !block_size.is_power_of_two() {
            panic!("block size must be a power of two");
        }

        let block_mask = ((block_size as u64) << 1) - 1;
        let max_extent_size = 0x7FFF_FFFF & !(block_mask as usize);

        Self {
            extents: Vec::new(),
            block_size,
            block_mask,
            max_extent_size,
        }
    }

    pub fn size(&self) -> u64 {
        self.extents
            .last()
            .map(|e| e.offset + e.data.len() as u64)
            .unwrap_or(0)
    }

    /// A binary search which allows choosing whether the lower or higher key should be returned when
    /// there's no exact match.
    ///
    /// Returns -1 if `offset` is smaller than the first extent (eg when first writing to offset
    /// 4096 and then to 0).
    /// Returns `self.size()` if `offset` is past the currently written data.
    /// Returns an index otherwise.
    fn search_extent(&self, offset: u64, bias: SearchBias) -> Result<usize, isize> {
        let mut a = -1isize;
        let mut b = self.extents.len() as isize;

        while (b - a) > 1 {
            let i = a + (b - a) / 2; // since `(a + b)/2` might overflow... in theory
            let entry_ofs = self.extents[i as usize].offset;
            if offset < entry_ofs {
                b = i;
            } else if offset > entry_ofs {
                a = i;
            } else {
                return Ok(i as usize);
            }
        }

        Err(match bias {
            SearchBias::Low => a,
            SearchBias::High => b,
        })
    }

    fn get_read_extents(&self, offset: u64) -> &[Extent] {
        match self.search_extent(offset, SearchBias::Low) {
            Ok(index) => &self.extents[index..],
            Err(beg) => {
                assert!(beg >= -1);
                let beg = beg.max(0) as usize;
                assert!((beg as u64) <= self.size());
                &self.extents[beg..]
            }
        }
    }

    pub fn read(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<usize> {
        let end = offset + buf.len() as u64;
        if end > self.size() {
            let remaining = self.size() - offset;
            buf = &mut buf[..(remaining as usize)]
        }
        let return_size = buf.len();

        for extent in self.get_read_extents(offset) {
            let data = if extent.offset <= offset {
                // in the first one we may be starting in the middle:
                let inside = (offset - extent.offset) as usize;
                &extent.data[inside..]
            } else {
                let empty_len = extent.offset - offset;
                if empty_len > (buf.len() as u64) {
                    break;
                }
                let (to_clear, remaining) = buf.split_at_mut(empty_len as usize);
                unsafe {
                    std::ptr::write_bytes(to_clear.as_mut_ptr(), 0, to_clear.len());
                }
                offset += to_clear.len() as u64;
                buf = remaining;
                &extent.data[..]
            };

            let data_len = data.len().min(buf.len());
            let (to_write, remaining) = buf.split_at_mut(data_len);
            to_write.copy_from_slice(&data[..data_len]);
            offset += to_write.len() as u64;
            buf = remaining;
        }

        // clear the remaining buffer with zeroes:
        unsafe {
            std::ptr::write_bytes(buf.as_mut_ptr(), 0, buf.len());
        }

        Ok(return_size)
    }

    pub fn truncate(&mut self, size: u64) {
        match self.search_extent(size, SearchBias::Low) {
            Ok(index) => self.extents.truncate(index),
            Err(-1) => self.extents.truncate(0),
            Err(index) => {
                let index = index as usize;
                self.extents.truncate(index + 1);
                let begin = self.extents[index].offset;
                self.extents[index].data.truncate((size - begin) as usize);
            }
        }
    }

    pub fn write(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        let block_mask_usize = self.block_mask as usize;
        if (buf.len() + block_mask_usize) & !block_mask_usize > self.max_extent_size {
            let mut offset = offset;
            for chunk in buf.chunks(self.max_extent_size) {
                self.write(chunk, offset)?;
                offset += chunk.len() as u64;
            }
            return Ok(());
        }

        let index = match self.search_extent(offset, SearchBias::Low) {
            Err(-1) => {
                // This is the very first extent to be written.
                let block_ofs = offset & !self.block_mask;
                let data_end_ofs = offset + buf.len() as u64;
                let extent_size = (data_end_ofs - block_ofs) as usize;
                let mut data = Vec::with_capacity(extent_size);
                let leading_zeros = (offset - block_ofs) as usize;
                unsafe {
                    data.set_len(extent_size);
                    let to_zero = &mut data[..leading_zeros];
                    std::ptr::write_bytes(to_zero.as_mut_ptr(), 0, to_zero.len());
                }
                data[leading_zeros..].copy_from_slice(buf);
                self.extents.push(Extent {
                    offset: block_ofs,
                    data,
                });
                return Ok(());
            }
            Ok(index) => index,
            Err(index) => {
                assert!(index >= 0);
                index as usize
            }
        };

        // We write in part to the extent at `index` and may want to merge with the extents
        // following it.

        let (extent, further_extents) = self.extents[index..].split_first_mut().unwrap();

        let block_mask_usize = self.block_mask as usize;
        let in_ofs = (offset - extent.offset) as usize;
        let in_end = in_ofs + buf.len();
        let in_block_end = (in_end + block_mask_usize) & !block_mask_usize;
        if in_block_end > self.max_extent_size {
            let possible = self.max_extent_size - in_ofs;
            self.write(&buf[..possible], offset)?;
            return self.write(&buf[possible..], offset + (possible as u64));
        }

        // at this point we know we will not exceed the maximum extent size:

        if extent.data.len() >= in_end {
            // we're not resizing the extent, so just wite and leave
            extent.data[in_ofs..in_end].copy_from_slice(buf);
            return Ok(());
        }

        // we definitely need to resize:
        let mut needed_end = in_end;
        if !further_extents.is_empty() {
            needed_end = (needed_end + block_mask_usize) & !block_mask_usize;
        }

        extent.data.reserve(needed_end - extent.data.len());
        unsafe {
            extent.data.set_len(needed_end);
            let to_zero = &mut extent.data[in_end..];
            std::ptr::write_bytes(to_zero.as_mut_ptr(), 0, to_zero.len());
        }

        extent.data[in_ofs..in_end].copy_from_slice(buf);

        let cur_end = extent.offset + extent.data.len() as u64;

        // data has been written, now handle the trailing extents:
        let next = match further_extents.first_mut() {
            None => return Ok(()),
            Some(next) => next,
        };

        if cur_end <= extent.offset {
            return Ok(());
        }

        let over_offset = extent.offset + (in_end as u64);
        let over_by = (over_offset - next.offset) as usize;
        let to_move_end = (over_by + block_mask_usize) & !block_mask_usize;
        let to_move_size = to_move_end - over_by;
        let extent_data_len = extent.data.len();
        extent.data[(extent_data_len - to_move_size)..].copy_from_slice(&next.data[..to_move_size]);
        next.offset += to_move_end as u64;
        next.data = next.data.split_off(to_move_end);
        Ok(())
    }
}
