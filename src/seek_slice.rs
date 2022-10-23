use std::io;
use std::io::{Read, Seek, SeekFrom};

pub struct SeekSlice<T: Seek> {
    inner: T,
    start: u64,
    end: u64,
    current: u64,
}

impl<T: Seek> SeekSlice<T> {
    pub fn new(mut inner: T, start: u64, end: u64) -> std::io::Result<SeekSlice<T>> {
        assert!(end >= start);
        // initialize current position to something sensible
        let current = inner.seek(SeekFrom::Start(start))?;
        Ok(SeekSlice {
            inner,
            start,
            end,
            current,
        })
    }
}

// should be a.checked_add_signed(b), but at time of writing, that won't be stable until
// the next rust release (any day now!).
fn checked_add_signed(a: u64, b: i64) -> Option<u64> {
    if b >= 0 {
        a.checked_add(b as u64)
    } else {
        // still wrong on i64::MIN, oh well
        a.checked_sub(b.abs() as u64)
    }
}

impl<T: Seek> Seek for SeekSlice<T> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let maybe_goal_idx = match pos {
            SeekFrom::Start(amount) => self.start.checked_add(amount),
            SeekFrom::End(amount) => checked_add_signed(self.end, amount),
            SeekFrom::Current(amount) => checked_add_signed(self.current, amount),
        };
        match maybe_goal_idx {
            Some(goal_idx) => {
                if goal_idx < self.start || goal_idx >= self.end {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid seek to a negative or overflowing position",
                    ))
                } else {
                    self.current = self.inner.seek(SeekFrom::Start(goal_idx))?;
                    Ok(self.current.checked_sub(self.start).unwrap())
                }
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "integer overflow while seeking",
            )),
        }
    }
}

impl<T: Read + Seek> Read for SeekSlice<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let max_read: usize =
            (self.end - self.current).try_into().unwrap_or(usize::MAX);
        let read_size = std::cmp::min(max_read, buf.len());
        let amount = self.inner.read(&mut buf[..read_size])?;
        self.current += amount as u64;
        Ok(amount)
    }
}

// could impl Write as well, but so far I haven't needed it

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_seek_slice() {
        let buf: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut cursor = Cursor::new(&buf);
        let mut slice = SeekSlice::new(&mut cursor, 2, 8).unwrap();
        // starts at offset zero
        assert_eq!(slice.seek(SeekFrom::Current(0)).unwrap(), 0);
        // reading advances position as expected
        fn next_byte<T: Read>(value: T) -> u8 {
            value.bytes().next().unwrap().unwrap()
        }
        assert_eq!(next_byte(&mut slice), 2u8);
        assert_eq!(next_byte(&mut slice), 3u8);
        assert_eq!(slice.seek(SeekFrom::Current(0)).unwrap(), 2);
        assert_eq!(next_byte(&mut slice), 4u8);

        // out of range seeks caught and have no effect
        assert!(slice.seek(SeekFrom::Current(-10)).is_err());
        assert!(slice.seek(SeekFrom::Current(10)).is_err());
        assert_eq!(next_byte(&mut slice), 5u8);

        assert_eq!(slice.seek(SeekFrom::Start(1)).unwrap(), 1);
        assert_eq!(next_byte(&mut slice), 3u8);

        assert_eq!(slice.seek(SeekFrom::End(-1)).unwrap(), 5);
        assert_eq!(next_byte(&mut slice), 7u8);
        assert!(slice.bytes().next().is_none());
    }
}
