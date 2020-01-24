use crate::{Point, RawPoint};
use crate::stream;
use crate::stream::raw::{self, Buffer};
use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::{mpsc, Arc, Mutex};

pub mod opt;

/// The function that will be called each time a new `Frame` is requested.
pub trait RenderFn<M>: Fn(&mut M, &mut Frame) {}
impl<M, F> RenderFn<M> for F where F: Fn(&mut M, &mut Frame) {}

/// A clone-able handle around a laser stream of frames.
pub struct Stream<M> {
    // A handle to the inner raw stream that drives this frame stream.
    raw: raw::Stream<M>,
    // A channel over which updates to the interpolation conf can be sent.
    state_update_tx: mpsc::Sender<StateUpdate>,
}

// State associated with the frame stream shared between the handle and laser stream.
#[derive(Clone)]
pub struct State {
    pub frame_hz: u32,
    pub interpolation_conf: opt::InterpolationConfig,
}

// Updates for the interpolation config sent from the stream handle to the laser thread.
type StateUpdate = Box<dyn FnMut(&mut State) + 'static + Send>;

/// A wrapper around the `Vec` of points being collected for the frame.
///
/// Provides a suite of methods that ease the process of submitting points.
///
/// Segments that contain more than one blank point in a row will be considered a blank segment.
pub struct Frame {
    frame_hz: u32,
    point_hz: u32,
    latency_points: u32,
    points: Vec<Point>,
}

// A type used for requesting frames from the user and feeding them to the raw buffer.
pub struct Requester {
    pub last_frame_point: Option<RawPoint>,
    pub raw_points: Vec<RawPoint>,
}

// The type of the default function used for the `process_raw` function if none is specified.
type DefaultProcessRawFn<M> = fn(&mut M, &mut Buffer);

/// A type allowing to build a raw laser stream.
pub struct Builder<M, F, R = DefaultProcessRawFn<M>> {
    /// The laser API inner state, used to find a DAC during `build` if one isn't specified.
    pub(crate) api_inner: Arc<crate::Inner>,
    pub builder: stream::Builder,
    pub model: M,
    pub render: F,
    pub process_raw: R,
    pub frame_hz: Option<u32>,
    pub interpolation_conf: opt::InterpolationConfigBuilder,
}

impl<M> Stream<M> {
    /// Update the `distance_per_point` field of the interpolation configuration.
    ///
    /// The value will be updated on the laser thread prior to requesting the next frame.
    ///
    /// Returns an `Err` if communication with the laser thread has been closed.
    pub fn set_distance_per_point(&self, d: f32) -> Result<(), mpsc::SendError<()>> {
        self.send_frame_state_update(move |state| state.interpolation_conf.distance_per_point = d)
            .map_err(|_| mpsc::SendError(()))
    }

    /// Update the `blank_delay_points` field of the interpolation configuration.
    ///
    /// The value will be updated on the laser thread prior to requesting the next frame.
    ///
    /// Returns an `Err` if communication with the laser thread has been closed.
    pub fn set_blank_delay_points(&self, ps: u32) -> Result<(), mpsc::SendError<()>> {
        self.send_frame_state_update(move |state| state.interpolation_conf.blank_delay_points = ps)
            .map_err(|_| mpsc::SendError(()))
    }

    /// Update the `radians_per_point` field of the interpolation configuration.
    ///
    /// The value will be updated on the laser thread prior to requesting the next frame.
    ///
    /// Returns an `Err` if communication with the laser thread has been closed.
    pub fn set_radians_per_point(&self, rad: f32) -> Result<(), mpsc::SendError<()>> {
        self.send_frame_state_update(move |state| state.interpolation_conf.radians_per_point = rad)
            .map_err(|_| mpsc::SendError(()))
    }

    /// Update the rate at which the stream will attempt to present images via the DAC.
    ///
    /// The value will be updated on the laser thread prior to requesting the next frame.
    ///
    /// Returns an `Err` if communication with the laser thread has been closed.
    pub fn set_frame_hz(&self, fps: u32) -> Result<(), mpsc::SendError<()>> {
        self.send_frame_state_update(move |state| state.frame_hz = fps)
            .map_err(|_| mpsc::SendError(()))
    }

    // Simplify sending a `StateUpdate` to the laser thread.
    fn send_frame_state_update<F>(&self, update: F) -> Result<(), mpsc::SendError<StateUpdate>>
    where
        F: FnOnce(&mut State) + Send + 'static,
    {
        let mut update_opt = Some(update);
        let update_fn = move |state: &mut State| {
            if let Some(update) = update_opt.take() {
                update(state);
            }
        };
        self.state_update_tx.send(Box::new(update_fn))
    }
}

impl<M, F, R> Builder<M, F, R> {
    /// The DAC with which the stream should be established.
    pub fn detected_dac(mut self, dac: crate::DetectedDac) -> Self {
        self.builder.dac = Some(dac);
        self
    }

    /// The initial rate at which the DAC should process points per second.
    ///
    /// This value should be no greater than the detected DAC's `max_point_hz`.
    ///
    /// By default this value is `stream::DEFAULT_POINT_HZ`.
    pub fn point_hz(mut self, point_hz: u32) -> Self {
        self.builder.point_hz = Some(point_hz);
        self
    }

    /// The initial rate at which the DAC should output frames per second.
    ///
    /// This in combination with the `point_hz` is used to determine the `points_per_frame`. Frames
    /// yielded by the user will be interpolated so that they always use exactly `points_per_frame`
    /// number of points per frame.
    ///
    /// By default, this value is `stream::DEFAULT_FRAME_HZ`.
    pub fn frame_hz(mut self, frame_hz: u32) -> Self {
        self.frame_hz = Some(frame_hz);
        self
    }

    /// The maximum latency specified as a number of points.
    ///
    /// Each time the laser indicates its "fullness", the raw stream will request enough points
    /// from the render function to fill the DAC buffer up to `latency_points`.
    pub fn latency_points(mut self, points: u32) -> Self {
        self.builder.latency_points = Some(points);
        self
    }

    /// The minimum distance the interpolator can travel along an edge before a new point is
    /// required.
    ///
    /// By default, this value is `InterpolationConfig::DEFAULT_DISTANCE_PER_POINT`.
    pub fn distance_per_point(mut self, dpp: f32) -> Self {
        self.interpolation_conf.distance_per_point = Some(dpp);
        self
    }

    /// The number of points to insert at the end of a blank to account for light modulator delay.
    ///
    /// By default, this value is `InterpolationConfig::DEFAULT_BLANK_DELAY_POINTS`.
    pub fn blank_delay_points(mut self, points: u32) -> Self {
        self.interpolation_conf.blank_delay_points = Some(points);
        self
    }

    /// The amount of delay to add based on the angle of the corner in radians.
    ///
    /// By default, this value is `InterpolationConfig::DEFAULT_RADIANS_PER_POINT`.
    pub fn radians_per_point(mut self, radians: f32) -> Self {
        self.interpolation_conf.radians_per_point = Some(radians);
        self
    }

    /// Specify a function that allows for processing the raw points before submission to the DAC.
    ///
    /// This mgiht be useful for:
    ///
    /// - applying post-processing effects onto the optimised, interpolated points.
    /// - monitoring the raw points resulting from the optimisation and interpolation processes.
    /// - tuning brightness of colours based on safety zones.
    ///
    /// The given function will get called right before submission of the optimised, interpolated
    /// buffer.
    pub fn process_raw<R2>(self, process_raw: R2) -> Builder<M, F, R2> {
        let Builder { api_inner, builder, model, render, frame_hz, interpolation_conf, .. } = self;
        Builder { api_inner, builder, model, render, process_raw, frame_hz, interpolation_conf }
    }

    /// Build the stream with the specified parameters.
    ///
    /// **Note:** If no `dac` was specified, this will method will block until a DAC is detected.
    /// The first detected DAC is the DAC with which a stream will be established.
    pub fn build(self) -> io::Result<Stream<M>>
    where
        M: 'static + Send,
        F: 'static + RenderFn<M> + Send,
        R: 'static + raw::RenderFn<M> + Send,
    {
        let Builder {
            api_inner,
            builder,
            model,
            render,
            process_raw,
            frame_hz,
            interpolation_conf,
        } = self;

        // Retrieve the interpolation configuration.
        let interpolation_conf = interpolation_conf.build();
        // Retrieve the frame rate to initialise the stream with.
        let frame_hz = frame_hz.unwrap_or(stream::DEFAULT_FRAME_HZ);

        // The type used for buffering frames and using them to serve points to the raw stream.
        let requester = Requester { last_frame_point: None, raw_points: vec![] };
        let requester = Arc::new(Mutex::new(requester));

        // A channel for updating the interpolation config.
        let (state_update_tx, state_update_rx) = mpsc::channel();
        let state_update_tx: mpsc::Sender<StateUpdate> = state_update_tx;

        // State to live on the stream thread.
        let state = Arc::new(Mutex::new(State { frame_hz, interpolation_conf }));

        // A render function for the inner raw stream.
        let raw_render = move |model: &mut M, buffer: &mut Buffer| {
            // Check for updates and retrieve a copy of the state.
            let state = {
                let mut state = state.lock().expect("failed to lock");
                for mut state_update in state_update_rx.try_iter() {
                    (*state_update)(&mut state);
                }
                state.clone()
            };

            let mut guard = requester.lock().expect("failed to lock frame requester");
            guard.fill_buffer(model, &render, buffer, &state);
            process_raw(model, buffer);
        };

        // Create the raw builder and build the raw stream.
        let raw_builder = raw::Builder { api_inner, builder, model, render: raw_render };
        let raw_stream = raw_builder.build()?;
        let stream = Stream { raw: raw_stream, state_update_tx };
        Ok(stream)
    }
}

impl Frame {
    /// The rate at which frames of points will be emitted by the DAC.
    pub fn frame_hz(&self) -> u32 {
        self.frame_hz
    }

    /// The rate at which these points will be emitted by the DAC.
    pub fn point_hz(&self) -> u32 {
        self.point_hz
    }

    /// The maximum number of points with which to fill the DAC's buffer.
    pub fn latency_points(&self) -> u32 {
        self.latency_points
    }

    /// The number of points emitted by the DAC per frame.
    pub fn points_per_frame(&self) -> u32 {
        self.point_hz / self.frame_hz
    }

    /// Add a sequence of consecutive points separated by blank space.
    ///
    /// If some points already exist in the frame, this method will create a blank segment between
    /// the previous point and the first point before appending this sequence.
    pub fn add_points<I>(&mut self, points: I)
    where
        I: IntoIterator,
        I::Item: AsRef<Point>,
    {
        for p in points {
            let p = *p.as_ref();
            self.add_lines([p, p].iter().cloned());
        }
    }

    /// Add a sequence of consecutive lines.
    ///
    /// If some points already exist in the frame, this method will create a blank segment between
    /// the previous point and the first point before appending this sequence.
    pub fn add_lines<I>(&mut self, points: I)
    where
        I: IntoIterator,
        I::Item: AsRef<Point>,
    {
        let mut points = points.into_iter();
        if let Some(&last) = self.points.last() {
            if let Some(next) = points.next() {
                let next = next.as_ref();
                self.points.push(last.blanked());
                self.points.push(next.blanked());
                self.points.push(*next);
            }
        }
        self.points.extend(points.map(|p| p.as_ref().clone()));
    }
}

impl Requester {
    // Fill the given buffer by requesting frames from the given user `render` function as
    // required.
    pub fn fill_buffer<M, F>(
        &mut self,
        model: &mut M,
        render: F,
        buffer: &mut Buffer,
        state: &State,
    )
    where
        F: RenderFn<M>,
    {
        // If the frame rate is `0`, leave the buffer empty.
        if state.frame_hz == 0 {
            return;
        }

        // If the buffer has no points, there's nothing to fill.
        if buffer.is_empty() {
            return;
        }

        // The number of points to generate per frame.
        let point_hz = buffer.point_hz();
        let latency_points = buffer.latency_points();

        // The starting index of the buffer we'll write to.
        let mut start = 0;

        // If there are still un-read points, use those first.
        if !self.raw_points.is_empty() {
            // If the pending range would not fill the buffer, write what we can.
            if self.raw_points.len() < buffer.len() {
                start = self.raw_points.len();
                buffer[..start].copy_from_slice(&self.raw_points);
                self.raw_points.clear();

            // If we have the exact number of frames as output, write them and return.
            } else if self.raw_points.len() == buffer.len() {
                buffer.copy_from_slice(&self.raw_points);
                self.raw_points.clear();
                return;

            // If we have too many points, write what we can and leave the rest.
            } else {
                let end = buffer.len();
                buffer.copy_from_slice(&self.raw_points[..end]);
                self.raw_points.drain(0..end);
                return;
            }
        }

        // The number of points to fill for each frame.
        let points_per_frame = point_hz / state.frame_hz;

        // If we reached this point, `self.raw_points` is empty so we should fill buffer with
        // frames until it is full.
        loop {
            // See how many points are left to fill.
            let num_points_remaining = buffer.len() - start;

            // Determine how many points to fill this pass.
            let num_points_to_fill = std::cmp::min(points_per_frame as usize, num_points_remaining);

            // Render a frame of points.
            let mut frame = Frame {
                point_hz,
                latency_points,
                frame_hz: state.frame_hz,
                points: vec![], // TODO: Reuse this buffer rather than allocating every loop.
            };
            render(model, &mut frame);

            // If we were given no points, the user must be expecting an empty frame.
            if frame.points.is_empty() {
                let blank_point = self.last_frame_point.map(|p| p.blanked())
                    .unwrap_or_else(RawPoint::centered_blank);
                self.raw_points.extend((0..points_per_frame).map(|_| blank_point));

            // Otherwise, we'll optimise and interpolate the given points.
            } else {
                // Optimisation passes.
                let segs = opt::points_to_segments(frame.iter().cloned());
                let pg = opt::segments_to_point_graph(segs);
                let eg = opt::point_graph_to_euler_graph(&pg);
                let ec = opt::euler_graph_to_euler_circuit(&eg);

                // Blank from last point of the previous frame to first point of this one.
                let last_frame_point = self.last_frame_point.take();
                let inter_frame_blank_points = match last_frame_point {
                    Some(last) => {
                        match eg.node_indices().next() {
                            None => vec![],
                            Some(next_id) => {
                                let next = eg[next_id];
                                if last.position != next.position {
                                    let a = last.blanked().with_weight(0);
                                    let b = next.to_raw().blanked();
                                    let blank_delay_points =
                                        state.interpolation_conf.blank_delay_points;
                                    opt::blank_segment_points(a, b, blank_delay_points).collect()
                                } else {
                                    vec![]
                                }
                            }
                        }
                    }
                    None => vec![],
                };

                // Subtract the inter-frame blank points from points per frame to maintain frame_hz.
                let inter_frame_point_count = inter_frame_blank_points.len() as u32;
                let target_points = if points_per_frame > inter_frame_point_count {
                    points_per_frame - inter_frame_point_count
                } else {
                    0
                };

                // Join the inter-frame points with the interpolated frame.
                let interp_conf = &state.interpolation_conf;
                let mut interpolated =
                    opt::interpolate_euler_circuit(&ec, &eg, target_points, interp_conf);

                // If the interpolated frame is empty there were no lit points or lines.
                // In this case, we'll produce an empty frame.
                if interpolated.is_empty() {
                    let blank_point = inter_frame_blank_points.last()
                        .map(|&p| p)
                        .or_else(|| last_frame_point.map(|p| p.blanked()))
                        .unwrap_or_else(RawPoint::centered_blank);
                    interpolated.extend((0..target_points).map(|_| blank_point));
                }

                self.raw_points.extend(inter_frame_blank_points);
                self.raw_points.extend(interpolated);
            }

            // Update the last frame point.
            self.last_frame_point = self.raw_points.last().map(|&p| p);

            // Write the points to buffer.
            let end = start + num_points_to_fill;
            let range = start..end;
            buffer[range.clone()].copy_from_slice(&self.raw_points[..range.len()]);
            self.raw_points.drain(..range.len());

            // If this output filled the buffer, break.
            if end == buffer.len() {
                break;
            }

            // Continue looping through the next frames.
            start = end;
        }
    }
}

impl Deref for Frame {
    type Target = Vec<Point>;
    fn deref(&self) -> &Self::Target {
        &self.points
    }
}

impl DerefMut for Frame {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.points
    }
}

impl<M> Deref for Stream<M> {
    type Target = raw::Stream<M>;
    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

// The default function used for the `process_raw` function if none is specified.
pub(crate) fn default_process_raw_fn<M>(_model: &mut M, _buffer: &mut Buffer) {
}
