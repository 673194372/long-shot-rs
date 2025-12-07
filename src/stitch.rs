//! Image stitching using OpenCV.
//! Implements Sobel gradient + template matching algorithm for scroll-based stitching.

use crate::types::{RawFrame, StitchParams};
use anyhow::{Result, anyhow};
use log::{debug, info};
use opencv::core::{
    self, Mat, MatTraitConst, MatTraitConstManual, Point, Rect, Size, CV_8UC4,
    BORDER_DEFAULT, AlgorithmHint,
};
use opencv::imgproc::{self, COLOR_BGRA2GRAY, TM_CCOEFF_NORMED};

/// Image stitcher for long screenshots
pub struct ImageStitcher {
    params: StitchParams,
    /// Accumulated result image (BGRA format)
    result: Option<Mat>,
    /// Previous frame for matching (gradient image)
    prev_gradient: Option<Mat>,
    /// Previous frame raw for reference
    prev_raw: Option<Mat>,
    /// Total height of stitched image
    total_height: i32,
    /// Last match Y position (for inertia constraint)
    last_match_y: Option<i32>,
    /// Frame counter
    frame_count: u32,
}

impl ImageStitcher {
    pub fn new(params: StitchParams) -> Self {
        Self {
            params,
            result: None,
            prev_gradient: None,
            prev_raw: None,
            total_height: 0,
            last_match_y: None,
            frame_count: 0,
        }
    }

    /// Reset the stitcher state
    pub fn reset(&mut self) {
        self.result = None;
        self.prev_gradient = None;
        self.prev_raw = None;
        self.total_height = 0;
        self.last_match_y = None;
        self.frame_count = 0;
    }

    /// Convert raw frame to OpenCV Mat (BGRA)
    fn frame_to_mat(frame: &RawFrame) -> Result<Mat> {
        let mat = unsafe {
            Mat::new_rows_cols_with_data_unsafe(
                frame.height as i32,
                frame.width as i32,
                CV_8UC4,
                frame.data.as_ptr() as *mut _,
                frame.stride as usize,
            )?
        };
        // Clone to own the data
        let mut owned = Mat::default();
        mat.copy_to(&mut owned)?;
        Ok(owned)
    }

    /// Convert Mat to grayscale
    fn to_grayscale(mat: &Mat) -> Result<Mat> {
        let mut gray = Mat::default();
        imgproc::cvt_color(mat, &mut gray, COLOR_BGRA2GRAY, 0, AlgorithmHint::ALGO_HINT_DEFAULT)?;
        Ok(gray)
    }

    /// Apply Sobel gradient (helps with low-contrast content like white backgrounds)
    fn compute_gradient(gray: &Mat) -> Result<Mat> {
        // Sobel X gradient
        let mut grad_x = Mat::default();
        imgproc::sobel(
            gray,
            &mut grad_x,
            core::CV_16S,
            1,
            0,
            3,
            1.0,
            0.0,
            BORDER_DEFAULT,
        )?;

        // Sobel Y gradient
        let mut grad_y = Mat::default();
        imgproc::sobel(
            gray,
            &mut grad_y,
            core::CV_16S,
            0,
            1,
            3,
            1.0,
            0.0,
            BORDER_DEFAULT,
        )?;

        // Convert to absolute values and combine
        let mut abs_grad_x = Mat::default();
        let mut abs_grad_y = Mat::default();
        core::convert_scale_abs(&grad_x, &mut abs_grad_x, 1.0, 0.0)?;
        core::convert_scale_abs(&grad_y, &mut abs_grad_y, 1.0, 0.0)?;

        // Weighted combination
        let mut gradient = Mat::default();
        core::add_weighted(&abs_grad_x, 0.5, &abs_grad_y, 0.5, 0.0, &mut gradient, -1)?;

        Ok(gradient)
    }

    /// Extract the effective region (ignoring top/bottom margins)
    fn get_effective_region(&self, height: i32) -> (i32, i32) {
        let ignore_top = (height as f64 * self.params.ignore_y_top) as i32;
        let ignore_bottom = (height as f64 * self.params.ignore_y_bottom) as i32;
        let effective_top = ignore_top;
        let effective_bottom = height - ignore_bottom;
        (effective_top, effective_bottom)
    }

    /// Extract template from top portion of current frame
    fn extract_template(&self, gradient: &Mat) -> Result<Mat> {
        let height = gradient.rows();
        let width = gradient.cols();

        let (effective_top, effective_bottom) = self.get_effective_region(height);
        let effective_height = effective_bottom - effective_top;

        // Template is top portion of effective region
        let template_height = (effective_height as f64 * self.params.template_ratio) as i32;
        let template_height = template_height.max(20); // Minimum template size

        let roi = Rect::new(0, effective_top, width, template_height);
        let template = Mat::roi(gradient, roi)?;

        let mut owned = Mat::default();
        template.copy_to(&mut owned)?;

        Ok(owned)
    }

    /// Find best match position using template matching with inertia constraint
    fn find_match(&self, template: &Mat, search_image: &Mat) -> Result<Option<(i32, f64)>> {
        let search_height = search_image.rows();
        let template_height = template.rows();

        if template_height >= search_height {
            return Ok(None);
        }

        // Define search region with inertia constraint
        let (search_top, search_bottom) = if let Some(last_y) = self.last_match_y {
            // Constrain search around last match position
            let min_y = (last_y - self.params.max_search_range).max(0);
            let max_y = (last_y + self.params.max_search_range).min(search_height - template_height);
            (min_y, max_y + template_height)
        } else {
            // First match: search entire effective region
            let (eff_top, eff_bottom) = self.get_effective_region(search_height);
            (eff_top, eff_bottom)
        };

        if search_bottom <= search_top + template_height {
            return Ok(None);
        }

        // Extract search ROI
        let search_roi = Rect::new(
            0,
            search_top,
            search_image.cols(),
            search_bottom - search_top,
        );
        let search_region = Mat::roi(search_image, search_roi)?;

        // Perform template matching
        let mut result = Mat::default();
        imgproc::match_template(
            &search_region,
            template,
            &mut result,
            TM_CCOEFF_NORMED,
            &Mat::default(),
        )?;

        // Find maximum
        let mut min_val = 0.0;
        let mut max_val = 0.0;
        let mut min_loc = Point::default();
        let mut max_loc = Point::default();
        core::min_max_loc(
            &result,
            Some(&mut min_val),
            Some(&mut max_val),
            Some(&mut min_loc),
            Some(&mut max_loc),
            &Mat::default(),
        )?;

        debug!(
            "Template match: confidence={:.3}, y={}",
            max_val,
            max_loc.y + search_top
        );

        if max_val >= self.params.match_confidence {
            Ok(Some((max_loc.y + search_top, max_val)))
        } else {
            Ok(None)
        }
    }

    /// Process a new frame and stitch if appropriate
    pub fn process_frame(&mut self, frame: &RawFrame) -> Result<bool> {
        self.frame_count += 1;

        // Convert frame to Mat
        let current_mat = Self::frame_to_mat(frame)?;
        let gray = Self::to_grayscale(&current_mat)?;
        let gradient = Self::compute_gradient(&gray)?;

        // First frame: initialize result
        if self.result.is_none() {
            info!("Initializing stitcher with first frame");
            self.result = Some(current_mat.clone());
            self.prev_gradient = Some(gradient);
            self.prev_raw = Some(current_mat);
            self.total_height = frame.height as i32;
            return Ok(true);
        }

        // Extract template from current frame (top portion)
        let template = self.extract_template(&gradient)?;

        // Search for template in previous frame
        let prev_gradient = self.prev_gradient.as_ref().unwrap();
        let match_result = self.find_match(&template, prev_gradient)?;

        if let Some((match_y, confidence)) = match_result {
            // Calculate shift (how many new pixels at the bottom)
            let (effective_top, _) = self.get_effective_region(frame.height as i32);
            let shift = match_y - effective_top;

            debug!(
                "Match found: y={}, confidence={:.3}, shift={}",
                match_y, confidence, shift
            );

            if shift > 0 && shift < frame.height as i32 {
                // Append new content to result
                self.append_content(&current_mat, shift)?;
                self.last_match_y = Some(match_y);

                // Update previous frame
                self.prev_gradient = Some(gradient);
                self.prev_raw = Some(current_mat);

                info!(
                    "Frame {} stitched: shift={}, total_height={}",
                    self.frame_count, shift, self.total_height
                );
                return Ok(true);
            } else {
                debug!("Invalid shift value: {}", shift);
            }
        } else {
            debug!(
                "No confident match found for frame {}",
                self.frame_count
            );
        }

        // Update previous frame even if no match (handle large jumps)
        self.prev_gradient = Some(gradient);
        self.prev_raw = Some(current_mat);

        Ok(false)
    }

    /// Append new content from the bottom of the current frame
    fn append_content(&mut self, current: &Mat, shift: i32) -> Result<()> {
        let result = self.result.as_ref().unwrap();
        let current_height = current.rows();
        let current_width = current.cols();

        // New content is from (height - shift) to bottom
        let new_content_start = current_height - shift;
        if new_content_start < 0 || new_content_start >= current_height {
            return Ok(());
        }

        // Create new result with extended height
        let new_height = self.total_height + shift;
        
        // Get raw data from existing result and current frame
        let result_data = result.data_bytes()?;
        let current_data = current.data_bytes()?;
        
        let row_bytes = (current_width * 4) as usize; // 4 bytes per pixel (BGRA)
        
        // Build new image data
        let mut new_data = Vec::with_capacity((new_height * current_width * 4) as usize);
        
        // Copy existing result data
        let existing_bytes = (self.total_height as usize) * row_bytes;
        new_data.extend_from_slice(&result_data[..existing_bytes]);
        
        // Copy new content from current frame (from new_content_start to end)
        let new_content_offset = (new_content_start as usize) * row_bytes;
        let new_content_bytes = (shift as usize) * row_bytes;
        new_data.extend_from_slice(&current_data[new_content_offset..new_content_offset + new_content_bytes]);
        
        // Create new Mat from combined data
        let new_result = unsafe {
            Mat::new_rows_cols_with_data_unsafe(
                new_height,
                current_width,
                CV_8UC4,
                new_data.as_ptr() as *mut _,
                row_bytes,
            )?
        };
        
        // Clone to own the data
        let mut owned = Mat::default();
        new_result.copy_to(&mut owned)?;

        self.result = Some(owned);
        self.total_height = new_height;

        Ok(())
    }

    /// Get the current stitched result as RGBA bytes
    pub fn get_result_rgba(&self) -> Option<(Vec<u8>, u32, u32)> {
        let result = self.result.as_ref()?;
        let height = result.rows() as u32;
        let width = result.cols() as u32;

        // Convert BGRA to RGBA
        let data = result.data_bytes().ok()?;
        let mut rgba = Vec::with_capacity(data.len());

        for chunk in data.chunks(4) {
            if chunk.len() == 4 {
                rgba.push(chunk[2]); // R
                rgba.push(chunk[1]); // G
                rgba.push(chunk[0]); // B
                rgba.push(chunk[3]); // A
            }
        }

        Some((rgba, width, height))
    }

    /// Get a scaled-down preview for display
    pub fn get_preview(&self, max_height: u32) -> Option<(Vec<u8>, u32, u32)> {
        let result = self.result.as_ref()?;
        let height = result.rows() as u32;
        let width = result.cols() as u32;

        if height <= max_height {
            return self.get_result_rgba();
        }

        // Scale down
        let scale = max_height as f64 / height as f64;
        let new_width = (width as f64 * scale) as i32;
        let new_height = max_height as i32;

        let mut resized = Mat::default();
        imgproc::resize(
            result,
            &mut resized,
            Size::new(new_width, new_height),
            0.0,
            0.0,
            imgproc::INTER_AREA,
        )
        .ok()?;

        let data = resized.data_bytes().ok()?;
        let mut rgba = Vec::with_capacity(data.len());

        for chunk in data.chunks(4) {
            if chunk.len() == 4 {
                rgba.push(chunk[2]); // R
                rgba.push(chunk[1]); // G
                rgba.push(chunk[0]); // B
                rgba.push(chunk[3]); // A
            }
        }

        Some((rgba, new_width as u32, new_height as u32))
    }

    /// Save result to PNG file
    #[allow(dead_code)]
    pub fn save_to_file(&self, path: &str) -> Result<()> {
        let result = self.result.as_ref()
            .ok_or_else(|| anyhow!("No image to save"))?;

        // Convert BGRA to BGR for saving (OpenCV expects BGR for imwrite)
        let mut bgr = Mat::default();
        imgproc::cvt_color(result, &mut bgr, imgproc::COLOR_BGRA2BGR, 0, AlgorithmHint::ALGO_HINT_DEFAULT)?;

        let params = core::Vector::<i32>::new();
        opencv::imgcodecs::imwrite(path, &bgr, &params)?;

        info!("Saved image to: {}", path);
        Ok(())
    }

    /// Get current total height
    #[allow(dead_code)]
    pub fn get_total_height(&self) -> i32 {
        self.total_height
    }

    /// Get frame count
    #[allow(dead_code)]
    pub fn get_frame_count(&self) -> u32 {
        self.frame_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stitch_params_default() {
        let params = StitchParams::default();
        assert!((params.ignore_y_top - 0.15).abs() < 0.01);
        assert!((params.match_confidence - 0.5).abs() < 0.01);
    }
}
