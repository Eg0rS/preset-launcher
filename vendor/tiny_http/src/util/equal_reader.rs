use std::io::Read;
use std::io::Result as IoResult;
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};

/// A `Reader` that reads exactly the number of bytes from a sub-reader.
///
/// If the limit is reached, it returns EOF. If the limit is not reached
/// when the destructor is called, the remaining bytes will be read and
/// thrown away.
pub struct EqualReader<R>
where
    R: Read,
{
    reader: R,
    size: usize,
    last_read_signal: Sender<IoResult<()>>,
}

impl<R> EqualReader<R>
where
    R: Read,
{
    pub fn new(reader: R, size: usize) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        let (tx, rx) = channel();

        let r = EqualReader {
            reader,
            size,
            last_read_signal: tx,
        };

        (r, rx)
    }
}

impl<R> Read for EqualReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.size == 0 {
            return Ok(0);
        }

        let buf = if buf.len() < self.size {
            buf
        } else {
            &mut buf[..self.size]
        };

        match self.reader.read(buf) {
            Ok(len) => {
                self.size -= len;
                Ok(len)
            }
            err @ Err(_) => err,
        }
    }
}

/// Размер одного куска при дочитывании неиспользованного тела.
const DRAIN_CHUNK: usize = 8 * 1024;
/// Если неиспользованный остаток больше — не дочитываем (защита от огромного Content-Length).
const DRAIN_MAX: usize = 64 * 1024;

impl<R> Drop for EqualReader<R>
where
    R: Read,
{
    // PRESET-LAUNCHER PATCH: оригинал делал `vec![0; remaining_to_read]` — аллокация размером
    // с Content-Length клиента. Теперь читаем кусками по DRAIN_CHUNK и огромный остаток не дочитываем.
    fn drop(&mut self) {
        let mut remaining_to_read = self.size;

        if remaining_to_read > DRAIN_MAX {
            let e = std::io::Error::new(std::io::ErrorKind::InvalidData, "request body too large to drain");
            self.last_read_signal.send(Err(e)).ok();
            return;
        }

        let mut buf = [0u8; DRAIN_CHUNK];
        while remaining_to_read > 0 {
            let n = remaining_to_read.min(DRAIN_CHUNK);
            match self.reader.read(&mut buf[..n]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    break;
                }
                Ok(0) => {
                    self.last_read_signal.send(Ok(())).ok();
                    break;
                }
                Ok(other) => {
                    remaining_to_read -= other;
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::EqualReader;
    use std::io::Read;

    #[test]
    fn test_limit() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut string = String::new();
            equal_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, "hello");
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_not_enough() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            assert_eq!(vec[0], b'h');
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }
}

#[cfg(test)]
mod huge_tests {
    use super::EqualReader;
    use std::io::Cursor;

    #[test]
    fn huge_remaining_is_not_drained_or_allocated() {
        // Заявленный размер — почти usize::MAX/2: оригинал упал бы на аллокации.
        let mut src = Cursor::new(vec![1u8; 100]);
        let (r, rx) = EqualReader::new(&mut src, usize::MAX / 2);
        drop(r);
        assert!(rx.recv().unwrap().is_err());
        assert_eq!(src.position(), 0);
    }

    #[test]
    fn small_remaining_is_drained_in_chunks() {
        let mut src = Cursor::new(vec![1u8; 30_000]);
        {
            let (_r, _rx) = EqualReader::new(&mut src, 20_000);
        }
        assert_eq!(src.position(), 20_000);
    }
}
