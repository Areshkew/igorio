/// A bitmap in memory. Share it as `Arc<Image>`; never clone pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, RGBA, straight (not premultiplied) alpha, rows top to bottom.
    pub rgba: Vec<u8>,
}

impl Image {
    /// `None` when `rgba` has the wrong length or a side is zero.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        (width > 0 && height > 0 && rgba.len() == expected).then_some(Self { width, height, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_wrong_buffer_length() {
        assert!(Image::new(2, 2, vec![0; 16]).is_some());
        assert!(Image::new(2, 2, vec![0; 15]).is_none());
        assert!(Image::new(0, 2, vec![]).is_none());
    }
}
