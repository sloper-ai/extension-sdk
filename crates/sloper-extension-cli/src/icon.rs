use std::{
    io::Cursor,
    path::Path,
};

use super::{
    Error,
    read_bounded,
};

// Check unsigned icon size and dimensions before a publication request.
pub(super) fn read(path: &Path) -> Result<Vec<u8>, Error> {
    let bytes = read_bounded(path, 256 * 1024)?;
    let mut decoder = png::Decoder::new(Cursor::new(&bytes));
    decoder.set_limits(png::Limits {
        bytes: 8 * 1024 * 1024,
    });
    let mut reader = decoder.read_info()?;
    let info = reader.info();
    if info.width == 0 || info.width != info.height || info.width > 512 {
        return Err(Error::invalid(
            "icon must be a square PNG between 1 and 512 pixels per side",
        ));
    }
    let frames = info
        .animation_control
        .as_ref()
        .map_or(1, |animation| animation.num_frames);
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or_else(|| Error::invalid("icon decoded dimensions exceed their limit"))?
    ];
    for _ in 0..frames {
        reader.next_frame(&mut pixels)?;
    }
    reader.finish()?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let encoder = png::Encoder::new(&mut bytes, width, height);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&vec![0; (width * height) as usize]).unwrap();
        }
        bytes
    }

    #[test]
    fn icons_require_square_bounded_complete_png_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("icon.png");
        let valid = png(32, 32);
        fs::write(&path, &valid).unwrap();
        assert_eq!(read(&path).unwrap(), valid);
        fs::write(&path, png(32, 16)).unwrap();
        assert!(read(&path).is_err());
        fs::write(&path, png(513, 513)).unwrap();
        assert!(read(&path).is_err());
        fs::write(&path, &valid[..valid.len() - 5]).unwrap();
        assert!(read(&path).is_err());
    }
}
