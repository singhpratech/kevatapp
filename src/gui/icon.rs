//! The app mark, bundled into the binary.
//!
//! Two sizes for two jobs: the window/taskbar icon the desktop asks for, and a larger
//! source for the header mark so it stays crisp on a HiDPI display where 24 logical
//! points is 48 physical pixels.

/// Window and taskbar icon.
const WINDOW_PNG: &[u8] = include_bytes!("../../assets/icon/icon-64.png");
/// Header mark — deliberately larger than it is drawn, for HiDPI.
const HEADER_PNG: &[u8] = include_bytes!("../../assets/icon/icon-128.png");

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Straight (non-premultiplied) RGBA8.
    pub rgba: Vec<u8>,
}

/// Decode to RGBA8 regardless of how the PNG was written.
///
/// `EXPAND` turns palette and low-bit-depth images into full channels and promotes
/// `tRNS` to a real alpha channel; `STRIP_16` folds 16-bit samples down to 8. Between
/// them the output is always 8-bit RGB or RGBA, so the match below is exhaustive in
/// practice and does not have to carry a case for every PNG colour type.
fn decode(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());

    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        // EXPAND has already removed this case; bail rather than render garbage.
        png::ColorType::Indexed => return None,
    };

    Some(Image {
        width: info.width,
        height: info.height,
        rgba,
    })
}

pub fn window() -> Option<Image> {
    decode(WINDOW_PNG)
}

pub fn header() -> Option<Image> {
    decode(HEADER_PNG)
}
