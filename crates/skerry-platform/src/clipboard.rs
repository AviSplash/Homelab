//! System clipboard access through `arboard` (Windows, macOS, X11, and
//! Wayland compositors that support the data-control protocol).

use anyhow::Result;
use skerry_core::clipboard::{ClipContent, ClipboardProvider, ImageData};
use std::borrow::Cow;

pub struct SystemClipboard {
    inner: arboard::Clipboard,
}

impl SystemClipboard {
    pub fn new() -> Result<Self> {
        Ok(SystemClipboard { inner: arboard::Clipboard::new()? })
    }
}

impl ClipboardProvider for SystemClipboard {
    fn get(&mut self) -> Result<Option<ClipContent>> {
        // Prefer an image when one is present: screenshots often also carry
        // a text representation (a file name) that is less useful.
        match self.inner.get_image() {
            Ok(img) if img.width > 0 && img.height > 0 => {
                return Ok(Some(ClipContent::Image(ImageData {
                    width: img.width,
                    height: img.height,
                    rgba: img.bytes.into_owned(),
                })));
            }
            _ => {}
        }
        match self.inner.get_text() {
            Ok(t) if !t.is_empty() => Ok(Some(ClipContent::Text(t))),
            Ok(_) => Ok(None),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&mut self, content: &ClipContent) -> Result<()> {
        match content {
            ClipContent::Text(t) => self.inner.set_text(t.clone())?,
            ClipContent::Image(img) => self.inner.set_image(arboard::ImageData {
                width: img.width,
                height: img.height,
                bytes: Cow::Borrowed(&img.rgba),
            })?,
        }
        Ok(())
    }
}
