// Copyright (C) 2022 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::vosk_client::{Configuration, Transcript, WordInfo};
use async_tungstenite::tungstenite::error::Error as WsError;
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use atomic_refcell::AtomicRefCell;
use futures::channel::mpsc;
use futures::future::{abortable, AbortHandle};
use futures::prelude::*;
use futures::Sink;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{
    element_error, error_msg, gst_debug, gst_error, gst_info, gst_log, gst_trace, gst_warning,
    loggable_error,
};
use once_cell::sync::Lazy;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;
use tokio::runtime;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "vosk_transcriber",
        gst::DebugColorFlags::empty(),
        Some("Vosk transcription element"),
    )
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(5);
const DEFAULT_SERVER_ADDRESS: &str = "ws://localhost:2700";
const GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(100);

#[derive(Debug, Clone)]
struct Settings {
    /// Total time to allow for transcribing audio. How late the buffer we produce will be.
    latency: gst::ClockTime,

    /// The address of the gRPC server to connect to for transcription.
    server_address: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            latency: DEFAULT_LATENCY,
            server_address: DEFAULT_SERVER_ADDRESS.to_string(),
        }
    }
}

struct State {
    /// Flag to indicate if we are connected to the remote Vosk server.
    connected: bool,

    /// The current queued buffers to be processed in the next iteration
    buffers: VecDeque<gst::Buffer>,

    // The time we started processing the buffers
    start_time: Option<gst::ClockTime>,

    /// Flag to indicate that we need to send the required stream initialization events to the src pad
    send_events: bool,

    /// Flag to indicate that we need to send a discontinuity buffer in the src pad
    send_discontinuity: bool,

    /// The segment of the stream we are receiving buffers from (sink pad)
    in_segment: gst::FormattedSegment<gst::ClockTime>,

    /// The sequence number of the segment we are receiving buffers from (sink pad)
    seqnum: gst::Seqnum,

    /// Track the segment position of the stream we are sending in the src pad
    out_segment: gst::FormattedSegment<gst::ClockTime>,

    /// The channel to send messages to the Vosk server transmission task
    sender: Option<mpsc::Sender<Message>>,

    /// Handle to stop the continuous reception of messages from the Vosk server
    recv_abort_handle: Option<AbortHandle>,

    /// Handler to stop the continuous transmission of messages to the Vosk server
    send_abort_handle: Option<AbortHandle>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            connected: false,
            buffers: VecDeque::new(),
            start_time: None,
            send_events: true,
            send_discontinuity: true,
            in_segment: gst::FormattedSegment::new(),
            seqnum: gst::Seqnum::next(),
            out_segment: gst::FormattedSegment::new(),
            sender: None,
            recv_abort_handle: None,
            send_abort_handle: None,
        }
    }
}

type WsSink = Pin<Box<dyn Sink<Message, Error = WsError> + Send + Sync>>;

pub struct Transcriber {
    /// Pad that we will receive the audio buffers from
    srcpad: gst::Pad,

    /// Pad that we will send the text transcription buffers to
    sinkpad: gst::Pad,

    // The settings of the element
    settings: Mutex<Settings>,

    // The state of the element, it is resent every time the element restarts
    state: Mutex<State>,

    // The sink of the messages to the Vosk server
    ws_sink: AtomicRefCell<Option<WsSink>>,
}

impl Transcriber {
    fn dequeue(&self, element: &super::Transcriber) -> bool {
        let mut items = vec![];

        let now = match element.current_running_time() {
            Some(now) => now,
            None => {
                return true;
            }
        };

        let latency = self.settings.lock().unwrap().latency;

        let mut state = self.state.lock().unwrap();

        if state.start_time.is_none() {
            state.start_time = Some(now);
            state.out_segment.set_position(now);
        }

        let start_time = state.start_time.unwrap();
        let mut last_position = state.out_segment.position().unwrap();

        while let Some(buf) = state.buffers.front() {
            let pts = buf.pts().unwrap();
            gst_trace!(
                CAT,
                obj: element,
                "Checking now {} if item is ready for dequeuing, PTS {}, threshold {} vs {}",
                now,
                pts,
                pts + latency.saturating_sub(3 * GRANULARITY),
                now - start_time
            );

            // If buffer pts is from the time that has passed so far, we add it to be dequeued
            if pts + latency.saturating_sub(3 * GRANULARITY) < now - start_time {
                /* Safe unwrap, we know we have an item */
                let mut buf = state.buffers.pop_front().unwrap();

                {
                    let buf_mut = buf.get_mut().unwrap();

                    // Fixes the buffer's presentation time
                    buf_mut.set_pts(start_time + pts);
                }

                items.push(buf);
            } else {
                break;
            }
        }

        let seqnum = state.seqnum;

        drop(state);

        for mut buf in items.drain(..) {
            let mut pts = buf.pts().unwrap();
            let mut duration = buf.duration().unwrap();

            match pts.cmp(&last_position) {
                Ordering::Greater => {
                    // When the PTS is greater than the last position, we need to send a gap event
                    let gap_event = gst::event::Gap::builder(last_position)
                        .duration(pts - last_position)
                        .seqnum(seqnum)
                        .build();
                    gst_log!(CAT, "Pushing gap:    {} -> {}", last_position, pts);
                    if !self.srcpad.push_event(gap_event) {
                        return false;
                    }
                }
                Ordering::Less => {
                    let delta = last_position - pts;

                    gst_warning!(
                        CAT,
                        obj: element,
                        "Updating item PTS ({} < {}), consider increasing latency",
                        pts,
                        last_position
                    );

                    pts = last_position;
                    duration = duration.saturating_sub(delta);

                    {
                        let buf_mut = buf.get_mut().unwrap();

                        buf_mut.set_pts(pts);
                        buf_mut.set_duration(duration);
                    }
                }
                _ => (),
            }

            last_position = pts + duration;

            gst_debug!(CAT, "Pushing buffer: {} -> {}", pts, pts + duration);

            if self.srcpad.push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */
        gst_trace!(
            CAT,
            obj: element,
            "Checking now: {} if we need to push a gap, last_position: {}, threshold: {}",
            now,
            last_position,
            last_position + latency.saturating_sub(GRANULARITY)
        );

        if now > last_position + latency.saturating_sub(GRANULARITY) {
            let duration = now - last_position - latency.saturating_sub(GRANULARITY);

            let gap_event = gst::event::Gap::builder(last_position)
                .duration(duration)
                .seqnum(seqnum)
                .build();

            gst_log!(
                CAT,
                "Pushing gap:    {} -> {}",
                last_position,
                last_position + duration
            );

            last_position += duration;

            if !self.srcpad.push_event(gap_event) {
                return false;
            }
        }

        self.state
            .lock()
            .unwrap()
            .out_segment
            .set_position(last_position);

        true
    }

    fn enqueue(
        &self,
        element: &super::Transcriber,
        state: &mut State,
        transcription: &Vec<WordInfo>,
    ) {
        for item in transcription.iter() {
            let start_time = gst::ClockTime::from_nseconds((item.start * 1_000_000_000.0) as u64);
            let end_time = gst::ClockTime::from_nseconds((item.end * 1_000_000_000.0) as u64);

            // Should be sent now
            gst_debug!(
                CAT,
                obj: element,
                "Item is ready for queuing: \"{}\", PTS {}",
                item.word,
                start_time
            );

            let mut buf = gst::Buffer::from_mut_slice(item.word.clone().into_bytes());
            {
                let buf = buf.get_mut().unwrap();

                if state.send_discontinuity {
                    buf.set_flags(gst::BufferFlags::DISCONT);
                    state.send_discontinuity = false;
                }

                // The presentation time here is from the start of the media, not from the start of the stream
                buf.set_pts(start_time);
                buf.set_duration(end_time - start_time);
            }

            state.buffers.push_back(buf);
        }
    }

    fn loop_fn(
        &self,
        element: &super::Transcriber,
        receiver: &mut mpsc::Receiver<Message>,
    ) -> Result<(), gst::ErrorMessage> {
        let mut events = {
            let mut events = vec![];

            let mut state = self.state.lock().unwrap();

            // Events that are always sent when starting a stream on gst elements
            if state.send_events {
                events.push(
                    gst::event::StreamStart::builder("transcription")
                        .seqnum(state.seqnum)
                        .build(),
                );

                let caps = gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build();
                events.push(
                    gst::event::Caps::builder(&caps)
                        .seqnum(state.seqnum)
                        .build(),
                );

                events.push(
                    gst::event::Segment::builder(&state.out_segment)
                        .seqnum(state.seqnum)
                        .build(),
                );

                // Events are sent only once, so we set this to false
                state.send_events = false;
            }

            events
        };

        for event in events.drain(..) {
            gst_info!(CAT, obj: element, "Sending {:?}", event);
            self.srcpad.push_event(event);
        }

        // Process the responses from Vosk server and produce text buffers
        let process_next_server_response_future = async move {
            let msg = match receiver.next().await {
                Some(msg) => msg,
                // Sender was closed so we stop the task
                None => {
                    let _ = self.srcpad.pause_task();
                    return Ok(());
                }
            };

            match msg {
                Message::Text(payload) => {
                    gst_trace!(CAT, obj: element, "got payload: {}", payload,);

                    let transcript: Transcript = match serde_json::from_str(&payload) {
                        Ok(transcript) => transcript,
                        Err(_) => {
                            // The payload is still not a final transcript, so we just ignore it
                            return Ok(());
                        }
                    };

                    gst_debug!(
                        CAT,
                        obj: element,
                        "result: {}",
                        serde_json::to_string_pretty(&transcript.result).unwrap(),
                    );

                    let mut state = self.state.lock().unwrap();

                    self.enqueue(element, &mut state, &transcript.result);

                    Ok(())
                }

                _ => Ok(()),
            }
        };

        // Wrap in a timeout so we can push gaps regularly
        let timed_call_future = async move {
            match tokio::time::timeout(GRANULARITY.into(), process_next_server_response_future)
                .await
            {
                Err(_) => {
                    if !self.dequeue(element) {
                        gst_info!(CAT, obj: element, "Failed to push gap event, pausing");

                        let _ = self.srcpad.pause_task();
                    }
                    Ok(())
                }
                Ok(res) => {
                    if !self.dequeue(element) {
                        gst_info!(CAT, obj: element, "Failed to push gap event, pausing");

                        let _ = self.srcpad.pause_task();
                    }
                    res
                }
            }
        };

        let _enter = RUNTIME.enter();
        futures::executor::block_on(timed_call_future)
    }

    /// Start task that will be called once to initialize the element
    fn start_task(&self, element: &super::Transcriber) -> Result<(), gst::LoggableError> {
        let (sender, mut receiver) = mpsc::channel(1);
        {
            let mut state = self.state.lock().unwrap();
            state.sender = Some(sender);
        }

        // This task is called repeatedly to produce text buffers and stream events.
        let res = self.srcpad.start_task({
            let element_weak = element.downgrade();
            let pad_weak = self.srcpad.downgrade();
            move || {
                let element = match element_weak.upgrade() {
                    Some(element) => element,
                    None => {
                        if let Some(pad) = pad_weak.upgrade() {
                            let _ = pad.pause_task();
                        }
                        return;
                    }
                };

                let transcribe = element.imp();
                // Do the actual work, of producing buffers and events.
                if let Err(err) = transcribe.loop_fn(&element, &mut receiver) {
                    element_error!(
                        &element,
                        gst::StreamError::Failed,
                        ["Streaming failed: {}", err]
                    );
                    let _ = transcribe.srcpad.pause_task();
                }
            }
        });
        if res.is_err() {
            return Err(loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn src_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &super::Transcriber,
        _mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            self.start_task(element)?;
        } else {
            {
                let mut state = self.state.lock().unwrap();
                state.sender = None;
            }

            let _ = self.srcpad.stop_task();
        }

        Ok(())
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::Transcriber,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Latency(mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                // Adds our own latency to the upstream peer's latency
                if ret {
                    let (_, min, _) = peer_query.result();
                    let our_latency = self.settings.lock().unwrap().latency;
                    gst_info!(CAT, obj: element, "Replying to latency query: {}", our_latency + min);
                    // We never drop buffers, so our max latency is set to infinity
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            QueryView::Position(mut q) => {
                if q.format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    q.set(
                        state
                            .out_segment
                            .to_stream_time(state.out_segment.position()),
                    );
                    true
                } else {
                    false
                }
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::Transcriber, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Eos(_) => match self.handle_buffer(pad, element, None) {
                Err(err) => {
                    gst_error!(CAT, "Failed to send EOS: {}", err);
                    false
                }
                Ok(_) => true,
            },
            EventView::FlushStart(_) => {
                gst_info!(CAT, obj: element, "Received flush start, disconnecting");
                let mut ret = pad.event_default(Some(element), event);

                match self.srcpad.stop_task() {
                    Err(err) => {
                        gst_error!(CAT, obj: element, "Failed to stop srcpad task: {}", err);

                        self.disconnect(element);

                        ret = false;
                    }
                    Ok(_) => {
                        self.disconnect(element);
                    }
                };

                ret
            }
            EventView::FlushStop(_) => {
                gst_info!(CAT, obj: element, "Received flush stop, restarting task");

                if pad.event_default(Some(element), event) {
                    match self.start_task(element) {
                        Err(err) => {
                            gst_error!(CAT, obj: element, "Failed to start srcpad task: {}", err);
                            false
                        }
                        Ok(_) => true,
                    }
                } else {
                    false
                }
            }
            EventView::Segment(e) => {
                let segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        element_error!(
                            element,
                            gst::StreamError::Format,
                            ["Only Time segments supported, got {:?}", segment.format(),]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                let mut state = self.state.lock().unwrap();

                state.in_segment = segment;
                state.seqnum = e.seqnum();

                true
            }
            EventView::Tag(_) => true,
            EventView::Caps(e) => {
                gst_info!(CAT, "Received caps {:?}", e);
                true
            }
            EventView::StreamStart(_) => true,
            _ => pad.event_default(Some(element), event),
        }
    }

    async fn sync_and_send(
        &self,
        element: &super::Transcriber,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut delay = None;

        {
            let state = self.state.lock().unwrap();

            if let Some(buffer) = &buffer {
                let running_time = state.in_segment.to_running_time(buffer.pts());
                let now = element.current_running_time();

                delay = running_time.opt_checked_sub(now).ok().flatten();
            }
        }

        // Wait until now is close enough to the buffer's PTS
        if let Some(delay) = delay {
            tokio::time::sleep(delay.into()).await;
        }

        if let Some(ws_sink) = self.ws_sink.borrow_mut().as_mut() {
            if let Some(buffer) = buffer {
                let data = buffer.map_readable().unwrap();
                for chunk in data.chunks(8000) {
                    ws_sink
                        .send(Message::Binary(chunk.to_vec()))
                        .await
                        .map_err(|err| {
                            gst_error!(CAT, obj: element, "Failed sending audio packet to server: {}", err);
                            gst::FlowError::Error
                        })?;
                }
                gst_trace!(CAT, obj: element, "Sent complete buffer to server!");
            } else {
                // Send end of stream
                gst_info!(CAT, obj: element, "Closing transcription session to Vosk server");
                ws_sink
                    .send(Message::Text("{\"eof\": 1}".to_string()))
                    .await
                    .map_err(|err| {
                        gst_error!(CAT, obj: element, "Failed sending EOF packet to server: {}", err);
                        gst::FlowError::Error
                    })?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_buffer(
        &self,
        _pad: &gst::Pad,
        element: &super::Transcriber,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: element, "Handling {:?}", buffer);

        self.ensure_connection(element).map_err(|err| {
            element_error!(
                element,
                gst::StreamError::Failed,
                ["Streaming failed: {}", err]
            );
            gst::FlowError::Error
        })?;

        let (send_handle, abort_handle) = abortable(self.sync_and_send(element, buffer));

        self.state.lock().unwrap().send_abort_handle = Some(abort_handle);

        let res = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(send_handle)
        };

        match res {
            Err(_) => Err(gst::FlowError::Flushing),
            Ok(res) => res,
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::Transcriber,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.handle_buffer(pad, element, Some(buffer))
    }

    fn ensure_connection(&self, element: &super::Transcriber) -> Result<(), gst::ErrorMessage> {
        {
            let state = self.state.lock().unwrap();

            if state.connected {
                return Ok(());
            }
        }

        let settings = self.settings.lock().unwrap();
        if settings.latency <= 2 * GRANULARITY {
            gst_error!(
                CAT,
                obj: element,
                "latency must be greater than 200 milliseconds"
            );
            return Err(error_msg!(
                gst::LibraryError::Settings,
                ["latency must be greater than 200 milliseconds"]
            ));
        }

        gst_info!(CAT, obj: element, "Connecting ..");

        let url = settings.server_address.clone();

        drop(settings);

        let (ws, _) = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(connect_async(url)).map_err(|err| {
                gst_error!(CAT, obj: element, "Failed to connect: {}", err);
                error_msg!(gst::CoreError::Failed, ["Failed to connect: {}", err])
            })?
        };

        let (ws_sink, mut ws_stream) = ws.split();

        *self.ws_sink.borrow_mut() = Some(Box::pin(ws_sink));

        let element_weak = element.downgrade();
        let recv_handle = async move {
            // First, send configuration to the Vosk server
            if let Some(element) = element_weak.upgrade() {
                let transcribe = element.imp();
                let mut sender = transcribe.state.lock().unwrap().sender.clone();

                // Set up the server to handle the incoming audio sample rate
                let in_caps = transcribe.sinkpad.current_caps().unwrap();
                let s = in_caps.structure(0).unwrap();
                let sample_rate = s.get::<i32>("rate").unwrap();

                gst_debug!(CAT, obj: &element, "Configuring transcription session, sample_rate={}", sample_rate);

                let config = Configuration::new(sample_rate);
                let packet = serde_json::to_vec(&config).unwrap();
                let msg = Message::Binary(packet);

                if let Some(sender) = sender.as_mut() {
                    if sender.send(msg).await.is_err() {
                        gst_error!(
                            CAT,
                            obj: &element,
                            "Failed to configure Vosk server for the expected sample rate",
                        );

                        return;
                    }
                } else {
                    return;
                }
            }

            while let Some(element) = element_weak.upgrade() {
                let transcribe = element.imp();
                let msg = match ws_stream.next().await {
                    Some(msg) => msg,
                    None => continue
                };

                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        gst_error!(CAT, "Failed to receive data: {}", err);
                        element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Streaming failed: {}", err]
                        );
                        break;
                    }
                };

                let mut sender = transcribe.state.lock().unwrap().sender.clone();

                if let Some(sender) = sender.as_mut() {
                    if let Err(err) = sender.send(msg).await {
                        gst_error!(CAT, obj: &element, "Stopped RECV handler: {}", err);
                        break;
                    }
                } else {
                    break;
                }
            }
        };

        let mut state = self.state.lock().unwrap();

        let (future, abort_handle) = abortable(recv_handle);

        state.recv_abort_handle = Some(abort_handle);

        RUNTIME.spawn(future);

        state.connected = true;

        gst_info!(CAT, obj: element, "Connected");

        Ok(())
    }

    fn disconnect(&self, element: &super::Transcriber) {
        let mut state = self.state.lock().unwrap();

        gst_info!(CAT, obj: element, "Unpreparing");

        if let Some(abort_handle) = state.recv_abort_handle.take() {
            abort_handle.abort();
        }

        if let Some(abort_handle) = state.send_abort_handle.take() {
            abort_handle.abort();
        }

        *state = State::default();

        gst_info!(
            CAT,
            obj: element,
            "Unprepared, connected: {}!",
            state.connected
        );
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "VoskTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |transcriber, element| transcriber.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| transcriber.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .activatemode_function(|pad, parent, mode, active| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(loggable_error!(CAT, "Panic activating src pad with mode")),
                    |transcriber, element| transcriber.src_activatemode(pad, element, mode, active),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| transcriber.src_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            ws_sink: AtomicRefCell::new(None),
        }
    }
}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::new(
                    "latency",
                    "Latency",
                    "Amount of milliseconds to allow Vosk to transcribe",
                    0,
                    std::u32::MAX,
                    DEFAULT_LATENCY.mseconds() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "server-address",
                    "Server Address",
                    "Address of the Vosk websocket server",
                    Some(DEFAULT_SERVER_ADDRESS),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                )
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "latency" => {
                let mut settings = self.settings.lock().unwrap();
                settings.latency = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "server-address" => {
                let mut settings = self.settings.lock().unwrap();
                settings.server_address = value.get().expect("type checked upstream")
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "server-address" => {
                let settings = self.settings.lock().unwrap();
                settings.server_address.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Transcriber {}

impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Audio/Text/Filter",
                "Speech to Text filter, using Vosk toolkit",
                "Rafael Caricio <rafael@caricio.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_caps = gst::Caps::builder("audio/x-raw")
                .field("format", "S16BE")
                .field("rate", gst::IntRange::new(8000_i32, 48000))
                .field("channels", 1)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_info!(CAT, obj: element, "Changing state {:?}", transition);

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                self.disconnect(element);
            }
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn provide_clock(&self, _element: &Self::Type) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
