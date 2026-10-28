//! 无平台 GIF 像素与全局调色板计算，供宿主独立配置热循环的优化级别。
pub mod frames;
pub mod palette;
pub mod pixels;

pub use palette::{Histogram, Palette};
pub use pixels::{valid_keys, Scaler};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[cfg(test)]
mod cached_pixels_tests;
