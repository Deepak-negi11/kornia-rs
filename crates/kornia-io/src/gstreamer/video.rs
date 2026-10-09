use super::{
    capture::StreamerState, error::VideoReaderError, set_location_property, StreamCapture,
    StreamCaptureError,
};
use gstreamer::prelude::*;
use kornia_image::{Image, ImageSize};
use std::{path::Path, time::Duration};

pub use gstreamer::SeekFlags;

/// The codec to use for the video writer.
pub enum VideoCodec {
    /// H.264 codec.
    H264,
}

/// The format of the image to write to the video file.
///
/// Usually will be the combination of the image format and the pixel type.
pub enum ImageFormat {
    /// 8-bit RGB format.
    Rgb8,
    /// 8-bit mono format.
    Mono8,
}

/// A struct for writing video files.
pub struct VideoWriter {
    pipeline: gstreamer::Pipeline,
    appsrc: gstreamer_app::AppSrc,
    fps: i32,
    format: ImageFormat,
    size: ImageSize,
    counter: u64,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl VideoWriter {
    /// Create a new VideoWriter.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to save the video file.
    /// * `codec` - The codec to use for the video writer.
    /// * `format` - The expected image format.
    /// * `fps` - The frames per second of the video.
    /// * `size` - The size of the video.
    pub fn new(
        path: impl AsRef<Path>,
        codec: VideoCodec,
        format: ImageFormat,
        fps: i32,
        size: ImageSize,
    ) -> Result<Self, StreamCaptureError> {
        // make sure that we do not initialize gstreamer several times
        if !gstreamer::INITIALIZED.load(std::sync::atomic::Ordering::Relaxed) {
            gstreamer::init()?;
        }

        // TODO: Add support for other codecs
        #[allow(unreachable_patterns)]
        let _codec = match codec {
            VideoCodec::H264 => "x264enc",
            _ => {
                return Err(StreamCaptureError::InvalidConfig(
                    "Unsupported codec".to_string(),
                ))
            }
        };

        // TODO: Add support for other formats
        let format_str = match format {
            ImageFormat::Mono8 => "GRAY8",
            ImageFormat::Rgb8 => "RGB",
        };

        if fps <= 0 {
            return Err(StreamCaptureError::InvalidConfig(format!(
                "fps must be positive, got {fps}"
            )));
        }

        // The output path is set as a property after parsing (see `set_location_property`).
        let pipeline_str = "appsrc name=src ! \
            videoconvert ! video/x-raw,format=I420 ! \
            x264enc ! \
            video/x-h264,profile=main ! \
            h264parse ! \
            mp4mux ! \
            filesink name=filesink";

        let pipeline = gstreamer::parse::launch(pipeline_str)?
            .dynamic_cast::<gstreamer::Pipeline>()
            .map_err(StreamCaptureError::DowncastPipelineError)?;
        set_location_property(&pipeline, "filesink", path.as_ref())?;

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| StreamCaptureError::GetElementByNameError)?
            .dynamic_cast::<gstreamer_app::AppSrc>()
            .map_err(StreamCaptureError::DowncastPipelineError)?;

        appsrc.set_format(gstreamer::Format::Time);

        let caps = gstreamer::Caps::builder("video/x-raw")
            .field("format", format_str)
            .field("width", size.width as i32)
            .field("height", size.height as i32)
            .field("framerate", gstreamer::Fraction::new(fps, 1))
            .build();

        appsrc.set_caps(Some(&caps));

        appsrc.set_is_live(true);
        appsrc.set_property("block", false);

        Ok(Self {
            pipeline,
            appsrc,
            fps,
            format,
            size,
            counter: 0,
            handle: None,
        })
    }

    /// Start the video writer.
    ///
    /// Set the pipeline to playing and launch a task to handle the bus messages.
    pub fn start(&mut self) -> Result<(), StreamCaptureError> {
        // set the pipeline to playing
        self.pipeline.set_state(gstreamer::State::Playing)?;

        let bus = self.pipeline.bus().ok_or(StreamCaptureError::BusError)?;

        // launch a task to handle the bus messages, exit when EOS is received and set the pipeline to null
        let handle = std::thread::spawn(move || {
            for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                match msg.view() {
                    gstreamer::MessageView::Eos(..) => {
                        log::debug!("gstreamer received EOS");
                        break;
                    }
                    gstreamer::MessageView::Error(err) => {
                        log::error!(
                            "Error from {:?}: {} ({:?})",
                            msg.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        );
                        break;
                    }
                    _ => {}
                }
            }
        });

        self.handle = Some(handle);

        Ok(())
    }

    ///  Close the video writer.
    ///
    /// Set the pipeline to null and join the thread.
    ///
    pub fn close(&mut self) -> Result<(), StreamCaptureError> {
        // send end of stream to the appsrc
        self.appsrc.end_of_stream()?;
        self.pipeline.set_state(gstreamer::State::Null)?;

        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                return Err(StreamCaptureError::JoinThreadError);
            }
        }

        Ok(())
    }
    /// Write an image to the video file.
    ///
    /// # Arguments
    ///
    /// * `img` - The image to write to the video file. It must have the size the writer was
    ///   created with, and 1 channel for [`ImageFormat::Mono8`] or 3 for [`ImageFormat::Rgb8`].
    ///
    /// # Returns
    ///
    /// `Ok(())` once the frame has been queued for encoding.
    ///
    /// # Errors
    ///
    /// Returns [`StreamCaptureError::InvalidImageFormat`] if the image has the wrong number of
    /// channels, [`StreamCaptureError::ImageError`] wrapping
    /// [`kornia_image::ImageError::InvalidImageSize`] if its size differs from the writer size,
    /// [`StreamCaptureError::GetBufferError`] if the frame buffer cannot be prepared, and
    /// [`StreamCaptureError::InvalidConfig`] if the frame cannot be pushed into the pipeline.
    // TODO: explore supporting write_async
    pub fn write<const C: usize>(&mut self, img: &Image<u8, C>) -> Result<(), StreamCaptureError> {
        check_frame(&self.format, self.size, img)?;

        let mut buffer = buffer_from_image(img, video_format(&self.format))?;

        let pts =
            gstreamer::ClockTime::from_nseconds(self.counter * 1_000_000_000 / self.fps as u64);
        let duration = gstreamer::ClockTime::from_nseconds(1_000_000_000 / self.fps as u64);

        let buffer_ref = buffer.get_mut().ok_or(StreamCaptureError::GetBufferError)?;
        buffer_ref.set_pts(Some(pts));
        buffer_ref.set_duration(Some(duration));

        self.counter += 1;

        if let Err(err) = self.appsrc.push_buffer(buffer) {
            return Err(StreamCaptureError::InvalidConfig(err.to_string()));
        }

        Ok(())
    }
}

/// Checks that `img` has the channel count of `format` and the writer `size`, so the pushed
/// buffer matches the caps.
///
/// # Errors
///
/// Returns [`StreamCaptureError::InvalidImageFormat`] if the channel count is wrong, and
/// [`StreamCaptureError::ImageError`] wrapping [`kornia_image::ImageError::InvalidImageSize`]
/// if the size is.
fn check_frame<const C: usize>(
    format: &ImageFormat,
    size: ImageSize,
    img: &Image<u8, C>,
) -> Result<(), StreamCaptureError> {
    let expected_channels = match format {
        ImageFormat::Mono8 => 1,
        ImageFormat::Rgb8 => 3,
    };
    if C != expected_channels {
        return Err(StreamCaptureError::InvalidImageFormat(format!(
            "Invalid number of channels: expected {expected_channels}, got {C}"
        )));
    }
    if img.size() != size {
        return Err(StreamCaptureError::ImageError(
            kornia_image::ImageError::InvalidImageSize(
                img.width(),
                img.height(),
                size.width,
                size.height,
            ),
        ));
    }
    Ok(())
}

/// The GStreamer pixel format matching an [`ImageFormat`].
fn video_format(format: &ImageFormat) -> gstreamer_video::VideoFormat {
    match format {
        ImageFormat::Mono8 => gstreamer_video::VideoFormat::Gray8,
        ImageFormat::Rgb8 => gstreamer_video::VideoFormat::Rgb,
    }
}

/// Copies `img` into a GStreamer buffer whose `VideoMeta` describes its tightly packed rows.
///
/// Raw video caps imply GStreamer's default layout, which pads every row to a multiple of
/// 4 bytes (an 854-pixel RGB row takes 2564 bytes, an 854-pixel GRAY8 row 856). Kornia images
/// are tightly packed, so without the meta, elements such as `videoconvert` cannot map the
/// frame at those widths and process an unwritten buffer instead (#1160).
///
/// # Errors
///
/// Returns [`StreamCaptureError::InvalidImageFormat`] if the image is too large to describe
/// with a `VideoMeta`.
fn buffer_from_image<const C: usize>(
    img: &Image<u8, C>,
    format: gstreamer_video::VideoFormat,
) -> Result<gstreamer::Buffer, StreamCaptureError> {
    let too_large =
        || StreamCaptureError::InvalidImageFormat(format!("image too large: {:?}", img.size()));
    let width = u32::try_from(img.width()).map_err(|_| too_large())?;
    let height = u32::try_from(img.height()).map_err(|_| too_large())?;
    let row_stride = img
        .width()
        .checked_mul(C)
        .and_then(|stride| i32::try_from(stride).ok())
        .ok_or_else(too_large)?;

    // TODO: verify is there is a cheaper way to copy the buffer
    let mut buffer = gstreamer::Buffer::from_mut_slice(img.as_slice().to_vec());
    let buffer_ref = buffer.get_mut().ok_or(StreamCaptureError::GetBufferError)?;
    gstreamer_video::VideoMeta::add_full(
        buffer_ref,
        gstreamer_video::VideoFrameFlags::empty(),
        format,
        width,
        height,
        &[0],
        &[row_stride],
    )
    .map_err(|e| StreamCaptureError::InvalidImageFormat(e.to_string()))?;
    Ok(buffer)
}

impl Drop for VideoWriter {
    fn drop(&mut self) {
        if self.handle.is_some() {
            if let Err(e) = self.close() {
                log::warn!("Failed to close video writer safely on drop: {:?}", e);
            }
        }
    }
}

/// A struct for reading video files
pub struct VideoReader(StreamCapture);

impl VideoReader {
    /// Creates a new `VideoReader`
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the video file to be read.
    /// * `format` - The expected image format.
    pub fn new(path: impl AsRef<Path>, format: ImageFormat) -> Result<Self, VideoReaderError> {
        // TODO: Support more formats
        let video_format = match format {
            ImageFormat::Rgb8 => "RGB",
            ImageFormat::Mono8 => "GRAY8",
        };

        // The input path is set as a property after parsing (see `set_location_property`);
        // the pipeline only starts reading once `start` sets it to PLAYING.
        let pipeline = format!(
            "filesrc name=filesrc ! \
            decodebin ! \
            videoconvert ! \
            video/x-raw,format={video_format} ! \
            appsink name=sink sync=true"
        );

        let capture = StreamCapture::new(&pipeline)?;
        set_location_property(&capture.pipeline, "filesrc", path.as_ref())?;

        Ok(Self(capture))
    }

    /// Starts the video reader pipeline
    #[inline]
    pub fn start(&mut self) -> Result<(), VideoReaderError> {
        self.0.start().map_err(VideoReaderError::StreamCaptureError)
    }

    /// Pauses the video reader pipeline
    #[inline]
    pub fn pause(&mut self) -> Result<(), VideoReaderError> {
        self.0
            .pipeline
            .set_state(gstreamer::State::Paused)
            .map_err(StreamCaptureError::from)?;
        Ok(())
    }

    /// Close the video reader pipeline
    #[inline]
    pub fn close(&self) -> Result<(), VideoReaderError> {
        self.0.close()?;
        Ok(())
    }

    /// Gets the current FPS of the video
    #[inline]
    pub fn get_fps(&self) -> Option<f64> {
        self.0.get_fps()
    }

    /// Grabs the oldest decoded frame as an RGB image.
    ///
    /// The reader must have been created with [`ImageFormat::Rgb8`]. As with
    /// [`StreamCapture::grab_rgb8`], the image is a read-only view of the GStreamer buffer when
    /// its rows are tightly packed and an owned copy when GStreamer padded them; call `.clone()`
    /// to get a writable image in either case.
    ///
    /// # Returns
    ///
    /// An Option containing the oldest decoded Image, or None if no frame is buffered yet.
    ///
    /// # Errors
    ///
    /// Returns [`VideoReaderError::StreamCaptureError`] wrapping
    /// [`StreamCaptureError::InvalidImageFormat`] if the reader was created with another format,
    /// or any other error of [`StreamCapture::grab_rgb8`].
    #[inline]
    pub fn grab_rgb8(&mut self) -> Result<Option<Image<u8, 3>>, VideoReaderError> {
        self.0
            .grab_rgb8()
            .map_err(VideoReaderError::StreamCaptureError)
    }

    /// Gets the current state of the video pipeline
    #[inline]
    pub fn get_state(&self) -> StreamerState {
        self.0.get_state()
    }

    /// Gets the current position in the video.
    ///
    /// # Returns
    ///
    /// * `Some(Duration)` - The current position as a Duration from the start of the video in nanoseconds
    /// * `None` - If the position could not be determined
    pub fn get_pos(&self) -> Option<Duration> {
        let clock_time = self
            .0
            .pipeline
            .query_position::<gstreamer::format::ClockTime>()?;

        let duration = Duration::from_nanos(clock_time.nseconds());
        Some(duration)
    }

    /// Gets the total duration of the video.
    ///
    /// # Returns
    ///
    /// * `Some(Duration)` - The total duration of the video
    /// * `None` - If the video duration could not be determined
    pub fn get_duration(&self) -> Option<Duration> {
        let clock_time = self
            .0
            .pipeline
            .query_duration::<gstreamer::format::ClockTime>()?;

        let duration = Duration::from_nanos(clock_time.nseconds());
        Some(duration)
    }

    /// Seeks to a specific position in the video.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position to seek to, as a Duration from the start of the video.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If the seek operation was successful.
    /// * `Err(VideoReaderError)` - If the seek operation failed.
    pub fn seek(
        &self,
        seek_flags: gstreamer::SeekFlags,
        pos: Duration,
    ) -> Result<(), VideoReaderError> {
        let pipeline = &self.0.pipeline;

        // Convert the Duration to ClockTime (nanoseconds)
        let clock_time = gstreamer::ClockTime::from_nseconds(pos.as_nanos() as u64);

        pipeline
            .seek_simple(seek_flags, clock_time)
            .map_err(|_| VideoReaderError::SeekError)
    }

    /// Sets the playback speed of the video.
    ///
    /// # Arguments
    ///
    /// * `speed` - The playback speed factor. 1.0 is normal speed, 0.5 is half speed, 2.0 is
    ///   double speed, etc.
    ///
    /// # Returns
    ///
    /// `true` if the speed change operation was successful, `false` otherwise.
    pub fn set_playback_speed(&self, speed: f64) -> Result<(), VideoReaderError> {
        if speed <= 0.0 {
            return Err(VideoReaderError::InvalidPlaybackSpeed); // Speed must be positive
        }

        let pipeline = &self.0.pipeline;

        // Get current position to maintain the playback position
        let position = pipeline
            .query_position::<gstreamer::format::ClockTime>()
            .ok_or(VideoReaderError::CurrentPosError)?;

        // Seek with the new rate
        pipeline
            .seek(
                speed,
                gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::ACCURATE,
                gstreamer::SeekType::Set,
                position,
                gstreamer::SeekType::None,
                gstreamer::ClockTime::NONE,
            )
            .map_err(|_| VideoReaderError::SeekError)
    }

    /// Resets the video to the beginning without changing its state.
    ///
    /// This function seeks the video to the origin (start) but does not stop or start the pipeline.
    pub fn reset(&self) -> Result<(), VideoReaderError> {
        let pipeline = &self.0.pipeline;
        pipeline
            .seek_simple(
                gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::ACCURATE,
                gstreamer::ClockTime::ZERO,
            )
            .map_err(|_| VideoReaderError::SeekError)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageFormat, VideoCodec, VideoReader, VideoWriter};
    use kornia_image::{Image, ImageSize};

    /// Regression: paths were quoted into the pipeline string, which rejected `\`
    /// (breaking Windows paths). The path must now reach `filesrc` verbatim.
    #[test]
    fn video_reader_accepts_any_path() -> Result<(), Box<dyn std::error::Error>> {
        use gstreamer::prelude::*;
        let path = std::path::Path::new(r"C:\my videos\clip ! fakesink name=x.mp4");
        let reader = VideoReader::new(path, ImageFormat::Rgb8)?;
        let src = reader
            .0
            .pipeline
            .by_name("filesrc")
            .ok_or("missing filesrc")?;
        assert_eq!(
            src.property::<Option<String>>("location").as_deref(),
            path.to_str()
        );
        Ok(())
    }

    /// An image whose bytes count up (mod 251, so no two rows of these sizes are equal).
    fn ramp<const C: usize>(
        width: usize,
        height: usize,
    ) -> Result<Image<u8, C>, Box<dyn std::error::Error>> {
        let data = (0..width * height * C).map(|i| (i % 251) as u8).collect();
        Ok(Image::new(ImageSize { width, height }, data)?)
    }

    /// Maps a writer buffer the way `videoconvert` does (`gst_video_frame_map` with the caps'
    /// `VideoInfo`) and checks that every row reads back as the image row.
    fn assert_maps_as_image<const C: usize>(
        format: gstreamer_video::VideoFormat,
        width: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use gstreamer_video::prelude::*;
        let img = ramp::<C>(width, 4)?;
        let info = gstreamer_video::VideoInfo::builder(format, width as u32, 4).build()?;
        let buffer = super::buffer_from_image(&img, format)?;
        let frame = gstreamer_video::VideoFrame::from_buffer_readable(buffer, &info)
            .map_err(|_| format!("{format} {width}x4: frame cannot be mapped"))?;
        let stride = frame.plane_stride()[0] as usize;
        assert_eq!(stride, width * C, "{format} {width}x4");
        let plane = frame.plane_data(0)?;
        for (y, row) in img.as_slice().chunks(width * C).enumerate() {
            assert_eq!(
                &plane[y * stride..y * stride + width * C],
                row,
                "{format} row {y}"
            );
        }
        Ok(())
    }

    /// Regression test for #1160 on the write side: at widths where rows would be padded
    /// (854 RGB and GRAY8), the tightly packed writer buffer must still map as the image.
    #[test]
    fn writer_buffers_map_as_tight_rows() -> Result<(), Box<dyn std::error::Error>> {
        gstreamer::init()?;
        for width in [854, 856] {
            assert_maps_as_image::<3>(gstreamer_video::VideoFormat::Rgb, width)?;
            assert_maps_as_image::<1>(gstreamer_video::VideoFormat::Gray8, width)?;
        }
        Ok(())
    }

    /// A writer buffer at an unaligned width goes through a real `videoconvert` (RGB to BGR,
    /// which is lossless) unchanged apart from the channel order.
    #[test]
    fn writer_frames_survive_videoconvert() -> Result<(), Box<dyn std::error::Error>> {
        use gstreamer::prelude::*;
        use gstreamer_video::prelude::*;
        const WIDTH: usize = 854;
        const HEIGHT: usize = 4;
        gstreamer::init()?;

        let pipeline = gstreamer::parse::launch(&format!(
            "appsrc name=src caps=video/x-raw,format=RGB,width={WIDTH},height={HEIGHT},framerate=30/1 ! \
             videoconvert ! video/x-raw,format=BGR ! appsink name=sink"
        ))?
        .dynamic_cast::<gstreamer::Pipeline>()
        .map_err(|_| "not a pipeline")?;
        let appsrc = pipeline
            .by_name("src")
            .ok_or("no appsrc")?
            .dynamic_cast::<gstreamer_app::AppSrc>()
            .map_err(|_| "not an appsrc")?;
        let appsink = pipeline
            .by_name("sink")
            .ok_or("no appsink")?
            .dynamic_cast::<gstreamer_app::AppSink>()
            .map_err(|_| "not an appsink")?;

        let img = ramp::<3>(WIDTH, HEIGHT)?;
        pipeline.set_state(gstreamer::State::Playing)?;
        appsrc.push_buffer(super::buffer_from_image(
            &img,
            gstreamer_video::VideoFormat::Rgb,
        )?)?;
        appsrc.end_of_stream()?;
        let sample = appsink.try_pull_sample(gstreamer::ClockTime::from_seconds(5));
        pipeline.set_state(gstreamer::State::Null)?;
        let sample = sample.ok_or("no frame out of videoconvert")?;

        let caps = sample.caps().ok_or("sample has no caps")?;
        let info = gstreamer_video::VideoInfo::from_caps(caps)?;
        let buffer = sample.buffer_owned().ok_or("sample has no buffer")?;
        let frame = gstreamer_video::VideoFrame::from_buffer_readable(buffer, &info)
            .map_err(|_| "output frame cannot be mapped")?;
        let stride = frame.plane_stride()[0] as usize;
        let plane = frame.plane_data(0)?;
        for (y, row) in img.as_slice().chunks(WIDTH * 3).enumerate() {
            let out = &plane[y * stride..y * stride + WIDTH * 3];
            for (x, (rgb, bgr)) in row.chunks(3).zip(out.chunks(3)).enumerate() {
                assert_eq!([rgb[2], rgb[1], rgb[0]], bgr, "pixel ({x}, {y})");
            }
        }
        Ok(())
    }

    /// `write` rejects frames that would not match the writer caps: a wrong channel count or
    /// a size other than the one the writer was created with.
    #[test]
    fn writer_rejects_frames_that_do_not_match_the_caps() -> Result<(), Box<dyn std::error::Error>>
    {
        let size = ImageSize {
            width: 854,
            height: 4,
        };
        super::check_frame(&ImageFormat::Rgb8, size, &ramp::<3>(854, 4)?)?;
        super::check_frame(&ImageFormat::Mono8, size, &ramp::<1>(854, 4)?)?;

        assert!(matches!(
            super::check_frame(&ImageFormat::Rgb8, size, &ramp::<1>(854, 4)?),
            Err(super::StreamCaptureError::InvalidImageFormat(_))
        ));
        assert!(matches!(
            super::check_frame(&ImageFormat::Mono8, size, &ramp::<3>(854, 4)?),
            Err(super::StreamCaptureError::InvalidImageFormat(_))
        ));
        assert!(matches!(
            super::check_frame(&ImageFormat::Rgb8, size, &ramp::<3>(856, 4)?),
            Err(super::StreamCaptureError::ImageError(
                kornia_image::ImageError::InvalidImageSize(856, 4, 854, 4)
            ))
        ));
        Ok(())
    }

    #[ignore = "need gstreamer in CI"]
    #[test]
    fn video_writer_rgb8u() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir()?;
        std::fs::create_dir_all(tmp_dir.path())?;

        let file_path = tmp_dir.path().join("test.mp4");

        let size = ImageSize {
            width: 6,
            height: 4,
        };

        let mut writer =
            VideoWriter::new(&file_path, VideoCodec::H264, ImageFormat::Rgb8, 30, size)?;
        writer.start()?;

        let img = Image::<u8, 3>::new(size, vec![0; size.width * size.height * 3])?;
        writer.write(&img)?;
        writer.close()?;

        assert!(file_path.exists(), "File does not exist: {file_path:?}");

        Ok(())
    }

    #[ignore = "need gstreamer in CI"]
    #[test]
    fn video_writer_mono8u() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir()?;
        std::fs::create_dir_all(tmp_dir.path())?;

        let file_path = tmp_dir.path().join("test.mp4");

        let size = ImageSize {
            width: 6,
            height: 4,
        };

        let mut writer =
            VideoWriter::new(&file_path, VideoCodec::H264, ImageFormat::Mono8, 30, size)?;
        writer.start()?;

        let img = Image::<u8, 1>::new(size, vec![0; size.width * size.height])?;
        writer.write(&img)?;
        writer.close()?;

        assert!(file_path.exists(), "File does not exist: {file_path:?}");

        Ok(())
    }
}
