//! Check declared leading metadata sizes before audio decoders allocate tag
//! buffers. Symphonia 0.5's ID3 reader ignores MetadataOptions limits.

use std::io::{Read, Seek, SeekFrom};

const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;

pub fn validate_metadata(reader: &mut (impl Read + Seek)) -> std::io::Result<()> {
    let result = validate(reader);
    reader.seek(SeekFrom::Start(0))?;
    result
}

fn invalid() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "слишком большие или повреждённые метаданные аудио")
}

fn validate(reader: &mut (impl Read + Seek)) -> std::io::Result<()> {
    let mut offset = 0u64;
    let mut metadata_bytes = 0u64;
    let mut blocks = 0;
    loop {
        blocks += 1;
        if blocks > 128 {
            return Err(invalid());
        }
        reader.seek(SeekFrom::Start(offset))?;
        let mut magic = [0u8; 4];
        if let Err(error) = reader.read_exact(&mut magic) {
            return if error.kind() == std::io::ErrorKind::UnexpectedEof { Ok(()) } else { Err(error) };
        }
        if &magic[..3] == b"ID3" {
            let mut rest = [0u8; 6];
            reader.read_exact(&mut rest)?;
            if !(2..=4).contains(&magic[3]) || rest[2..].iter().any(|byte| *byte & 0x80 != 0) {
                return Err(invalid());
            }
            let size = rest[2..].iter().fold(0u64, |size, byte| (size << 7) | u64::from(*byte));
            let footer = if magic[3] == 4 && rest[1] & 0x10 != 0 { 10 } else { 0 };
            metadata_bytes += 10 + size + footer;
            if metadata_bytes > MAX_METADATA_BYTES {
                return Err(invalid());
            }
            offset = metadata_bytes;
        } else if &magic == b"fLaC" {
            loop {
                blocks += 1;
                if blocks > 128 {
                    return Err(invalid());
                }
                let mut header = [0u8; 4];
                reader.read_exact(&mut header)?;
                let size = (u64::from(header[1]) << 16) | (u64::from(header[2]) << 8) | u64::from(header[3]);
                metadata_bytes += 4 + size;
                if metadata_bytes > MAX_METADATA_BYTES {
                    return Err(invalid());
                }
                if header[0] & 0x80 != 0 {
                    return Ok(());
                }
                reader.seek(SeekFrom::Current(size as i64))?;
            }
        } else {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_declared_binary_fields_are_refused_before_any_body_is_read() {
        use symphonia::core::io::ReadBytes;
        let mut reader = symphonia::core::io::BufReader::new(b"body");
        let error = reader.read_boxed_slice_exact(64 * 1024 * 1024).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(reader.pos(), 0);
        assert_eq!(reader.read_boxed_slice_exact(4).unwrap().as_ref(), b"body");
    }

    #[test]
    fn oversized_tags_are_refused_from_the_header_before_reading_the_body() {
        let mut huge_id3 = std::io::Cursor::new(b"ID3\x04\0\0\x7f\x7f\x7f\x7f");
        assert_eq!(validate_metadata(&mut huge_id3).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(huge_id3.position(), 0);
        let mut flac = std::io::Cursor::new(b"fLaC\x80\xff\xff\xff");
        assert_eq!(validate_metadata(&mut flac).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn valid_small_tags_and_ordinary_audio_are_rewound_for_the_decoder() {
        for bytes in
            [b"ID3\x04\0\0\0\0\0\x04testdata".as_slice(), b"fLaC\x80\0\0\x22", b"RIFFxxxxWAVE", b"\xff\xfb\x90\0"]
        {
            let mut source = std::io::Cursor::new(bytes);
            validate_metadata(&mut source).unwrap();
            assert_eq!(source.position(), 0);
        }
    }
}
