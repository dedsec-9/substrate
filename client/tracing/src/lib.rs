// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Instrumentation implementation for substrate.
//!
//! This crate is unstable and the API and usage may change.
//!
//! # Usage
//!
//! See `sp-tracing` for examples on how to use tracing.
//!
//! Currently we provide `Log` (default), `Telemetry` variants for `Receiver`

use rustc_hash::FxHashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::ser::{Serialize, Serializer, SerializeMap};
use slog::{SerdeValue, Value};
use tracing_core::{
	event::Event,
	field::{Visit, Field},
	Level,
	metadata::Metadata,
	span::{Attributes, Id, Record},
	subscriber::Subscriber,
};
use tracing_subscriber::CurrentSpan;

use sc_telemetry::{telemetry, SUBSTRATE_INFO};
use sp_tracing::proxy::{WASM_NAME_KEY, WASM_TARGET_KEY, WASM_TRACE_IDENTIFIER};

const ZERO_DURATION: Duration = Duration::from_nanos(0);
const PROXY_TARGET: &'static str = "sp_tracing::proxy";
// To ensure we don't accumulate too many associated events
const LEN_LIMIT: usize = 128;

/// Used to configure how to receive the metrics
#[derive(Debug, Clone)]
pub enum TracingReceiver {
	/// Output to logger
	Log,
	/// Output to telemetry
	Telemetry,
}

impl Default for TracingReceiver {
	fn default() -> Self {
		Self::Log
	}
}

/// A handler for tracing `SpanDatum`
pub trait TraceHandler: Send + Sync {
	/// Process a `SpanDatum`
	fn process_span(&self, span: SpanDatum);
	/// Process a `TraceEvent`
	fn process_event(&self, event: TraceEvent);
}

/// Represents a tracing event, complete with values
#[derive(Debug)]
pub struct TraceEvent {
	pub name: &'static str,
	pub target: String,
	pub level: Level,
	pub visitor: Visitor,
	pub parent_id: Option<u64>,
}

/// Represents a single instance of a tracing span
#[derive(Debug)]
pub struct SpanDatum {
	pub id: u64,
	pub parent_id: Option<u64>,
	pub name: String,
	pub target: String,
	pub level: Level,
	pub line: u32,
	pub start_time: Instant,
	pub overall_time: Duration,
	pub values: Visitor,
	pub events: Vec<TraceEvent>,
}

/// Responsible for assigning ids to new spans, which are not re-used.
pub struct ProfilingSubscriber {
	next_id: AtomicU64,
	targets: Vec<(String, Level)>,
	trace_handler: Box<dyn TraceHandler>,
	span_data: Mutex<FxHashMap<u64, SpanDatum>>,
	current_span: CurrentSpan,
}

/// Holds associated values for a tracing span
#[derive(Clone, Debug)]
pub struct Visitor(FxHashMap<String, String>);

impl Visitor {
	/// Consume the Visitor, returning the inner FxHashMap
	pub fn into_inner(self) -> FxHashMap<String, String> {
		self.0
	}
}

impl Visit for Visitor {
	fn record_i64(&mut self, field: &Field, value: i64) {
		self.0.insert(field.name().to_string(), value.to_string());
	}

	fn record_u64(&mut self, field: &Field, value: u64) {
		self.0.insert(field.name().to_string(), value.to_string());
	}

	fn record_bool(&mut self, field: &Field, value: bool) {
		self.0.insert(field.name().to_string(), value.to_string());
	}

	fn record_str(&mut self, field: &Field, value: &str) {
		self.0.insert(field.name().to_string(), value.to_owned());
	}

	fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
		self.0.insert(field.name().to_string(), format!("{:?}", value));
	}
}

impl Serialize for Visitor {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
		where S: Serializer,
	{
		let mut map = serializer.serialize_map(Some(self.0.len()))?;
		for (k, v) in &self.0 {
			map.serialize_entry(k, v)?;
		}
		map.end()
	}
}

impl fmt::Display for Visitor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let values = self.0.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<String>>().join(", ");
		write!(f, "{}", values)
	}
}

impl SerdeValue for Visitor {
	fn as_serde(&self) -> &dyn erased_serde::Serialize {
		self
	}

	fn to_sendable(&self) -> Box<dyn SerdeValue + Send + 'static> {
		Box::new(self.clone())
	}
}

impl Value for Visitor {
	fn serialize(
		&self,
		_record: &slog::Record,
		key: slog::Key,
		ser: &mut dyn slog::Serializer,
	) -> slog::Result {
		ser.emit_serde(key, self)
	}
}

impl ProfilingSubscriber {
	/// Takes a `TracingReceiver` and a comma separated list of targets,
	/// either with a level: "pallet=trace,frame=debug"
	/// or without: "pallet,frame" in which case the level defaults to `trace`.
	/// wasm_tracing indicates whether to enable wasm traces
	pub fn new(receiver: TracingReceiver, targets: &str) -> ProfilingSubscriber {
		match receiver {
			TracingReceiver::Log => Self::new_with_handler(Box::new(LogTraceHandler), targets),
			TracingReceiver::Telemetry => Self::new_with_handler(
				Box::new(TelemetryTraceHandler),
				targets,
			),
		}
	}

	/// Allows use of a custom TraceHandler to create a new instance of ProfilingSubscriber.
	/// Takes a comma separated list of targets,
	/// either with a level, eg: "pallet=trace"
	/// or without: "pallet" in which case the level defaults to `trace`.
	/// wasm_tracing indicates whether to enable wasm traces
	pub fn new_with_handler(trace_handler: Box<dyn TraceHandler>, targets: &str)
		-> ProfilingSubscriber
	{
		let targets: Vec<_> = targets.split(',').map(|s| parse_target(s)).collect();
		ProfilingSubscriber {
			next_id: AtomicU64::new(1),
			targets,
			trace_handler,
			span_data: Mutex::new(FxHashMap::default()),
			current_span: Default::default()
		}
	}

	fn check_target(&self, target: &str, level: &Level) -> bool {
		for t in &self.targets {
			if target.starts_with(t.0.as_str()) && level <= &t.1 {
				return true;
			}
		}
		false
	}
}

// Default to TRACE if no level given or unable to parse Level
// We do not support a global `Level` currently
fn parse_target(s: &str) -> (String, Level) {
	match s.find('=') {
		Some(i) => {
			let target = s[0..i].to_string();
			if s.len() > i {
				let level = s[i + 1..s.len()].parse::<Level>().unwrap_or(Level::TRACE);
				(target, level)
			} else {
				(target, Level::TRACE)
			}
		}
		None => (s.to_string(), Level::TRACE)
	}
}

impl Subscriber for ProfilingSubscriber {
	fn enabled(&self, metadata: &Metadata<'_>) -> bool {
		if metadata.target() == PROXY_TARGET || self.check_target(metadata.target(), metadata.level()) {
			log::debug!(target: "tracing", "Enabled target: {}, level: {}", metadata.target(), metadata.level());
			true
		} else {
			log::debug!(target: "tracing", "Disabled target: {}, level: {}", metadata.target(), metadata.level());
			false
		}
	}

	fn new_span(&self, attrs: &Attributes<'_>) -> Id {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let mut values = Visitor(FxHashMap::default());
		attrs.record(&mut values);
		// If this is a wasm trace, check if target/level is enabled
		if let Some(wasm_target) = values.0.get(WASM_TARGET_KEY) {
			if !self.check_target(wasm_target, attrs.metadata().level()) {
				return Id::from_u64(id);
			}
		}
		let span_datum = SpanDatum {
			id,
			parent_id: self.current_span.id().map(|p| p.into_u64()),
			name: attrs.metadata().name().to_owned(),
			target: attrs.metadata().target().to_owned(),
			level: attrs.metadata().level().clone(),
			line: attrs.metadata().line().unwrap_or(0),
			start_time: Instant::now(),
			overall_time: ZERO_DURATION,
			values,
			events: Vec::new(),
		};
		self.span_data.lock().insert(id, span_datum);
		Id::from_u64(id)
	}

	fn record(&self, span: &Id, values: &Record<'_>) {
		let mut span_data = self.span_data.lock();
		if let Some(s) = span_data.get_mut(&span.into_u64()) {
			values.record(&mut s.values);
		}
	}

	fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

	fn event(&self, event: &Event<'_>) {
		let mut visitor = Visitor(FxHashMap::default());
		event.record(&mut visitor);
		let trace_event = TraceEvent {
			name: event.metadata().name(),
			target: event.metadata().target().to_owned(),
			level: event.metadata().level().clone(),
			visitor,
			parent_id: self.current_span.id().map(|id| id.into_u64()),
		};
		// Q: Should all events be emitted immediately, rather than grouping with parent span?
		match trace_event.parent_id {
			Some(parent_id) => {
				if let Some(span) = self.span_data.lock().get_mut(&parent_id) {
					if span.events.len() > LEN_LIMIT {
						log::warn!(
							target: "tracing",
							"Accumulated too many events for span id: {}, sending event separately",
							parent_id
						);
						self.trace_handler.process_event(trace_event);
					} else {
						span.events.push(trace_event);
					}
				} else {
					log::warn!(
						target: "tracing",
						"Parent span missing"
					);
					self.trace_handler.process_event(trace_event);
				}
			}
			None => self.trace_handler.process_event(trace_event),
		}
	}

	fn enter(&self, span: &Id) {
		self.current_span.enter(span.clone());
		let mut span_data = self.span_data.lock();
		let start_time = Instant::now();
		if let Some(mut s) = span_data.get_mut(&span.into_u64()) {
			s.start_time = start_time;
		}
	}

	fn exit(&self, span: &Id) {
		self.current_span.exit();
		let end_time = Instant::now();
		let mut span_data = self.span_data.lock();
		if let Some(mut s) = span_data.get_mut(&span.into_u64()) {
			s.overall_time = end_time - s.start_time + s.overall_time;
		}
	}

	fn try_close(&self, span: Id) -> bool {
		let span_datum = {
			let mut span_data = self.span_data.lock();
			span_data.remove(&span.into_u64())
		};
		if let Some(mut span_datum) = span_datum {
			if span_datum.name == WASM_TRACE_IDENTIFIER {
				span_datum.values.0.insert("wasm".to_owned(), "true".to_owned());
				if let Some(n) = span_datum.values.0.remove(WASM_NAME_KEY) {
					span_datum.name = n;
				}
				if let Some(t) = span_datum.values.0.remove(WASM_TARGET_KEY) {
					span_datum.target = t;
				}
			}
			if self.check_target(&span_datum.target, &span_datum.level) {
				self.trace_handler.process_span(span_datum);
			}
		};
		true
	}
}

/// TraceHandler for sending span data to the logger
pub struct LogTraceHandler;

fn log_level(level: Level) -> log::Level {
	match level {
		Level::TRACE => log::Level::Trace,
		Level::DEBUG => log::Level::Debug,
		Level::INFO => log::Level::Info,
		Level::WARN => log::Level::Warn,
		Level::ERROR => log::Level::Error,
	}
}

impl TraceHandler for LogTraceHandler {
	fn process_span(&self, span_datum: SpanDatum) {
		if span_datum.values.0.is_empty() {
			log::log!(
				log_level(span_datum.level),
				"{}: {}, time: {}, id: {}, parent_id: {:?}, events: {:?}",
				span_datum.target,
				span_datum.name,
				span_datum.overall_time.as_nanos(),
				span_datum.id,
				span_datum.parent_id,
				span_datum.events,
			);
		} else {
			log::log!(
				log_level(span_datum.level),
				"{}: {}, time: {}, id: {}, parent_id: {:?}, values: {}, events: {:?}",
				span_datum.target,
				span_datum.name,
				span_datum.overall_time.as_nanos(),
				span_datum.id,
				span_datum.parent_id,
				span_datum.values,
				span_datum.events,
			);
		}
	}

	fn process_event(&self, event: TraceEvent) {
		log::log!(
			log_level(event.level),
			"{}: {}, parent_id: {:?}, values: {}",
			event.name,
			event.target,
			event.parent_id,
			event.visitor
		);
	}
}

/// TraceHandler for sending span data to telemetry,
/// Please see telemetry documentation for details on how to specify endpoints and
/// set the required telemetry level to activate tracing messages
pub struct TelemetryTraceHandler;

impl TraceHandler for TelemetryTraceHandler {
	fn process_span(&self, span_datum: SpanDatum) {
		telemetry!(SUBSTRATE_INFO; "tracing.span";
			"name" => span_datum.name,
			"target" => span_datum.target,
			"time" => span_datum.overall_time.as_nanos(),
			"id" => span_datum.id,
			"parent_id" => span_datum.parent_id,
			"values" => span_datum.values
		);
	}

	fn process_event(&self, event: TraceEvent) {
		telemetry!(SUBSTRATE_INFO; "tracing.event";
			"name" => event.name,
			"target" => event.target,
			"parent_id" => event.parent_id,
			"values" => event.visitor
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Arc;

	struct TestTraceHandler{
		spans: Arc<Mutex<Vec<SpanDatum>>>,
		events: Arc<Mutex<Vec<TraceEvent>>>,
	}

	impl TraceHandler for TestTraceHandler {
		fn process_span(&self, sd: SpanDatum) {
			self.spans.lock().push(sd);
		}

		fn process_event(&self, event: TraceEvent) {
			self.events.lock().push(event);
		}
	}

	fn setup_subscriber() -> (ProfilingSubscriber, Arc<Mutex<Vec<SpanDatum>>>, Arc<Mutex<Vec<TraceEvent>>>) {
		let spans = Arc::new(Mutex::new(Vec::new()));
		let events = Arc::new(Mutex::new(Vec::new()));
		let handler = TestTraceHandler {
			spans: spans.clone(),
			events: events.clone(),
		};
		let test_subscriber = ProfilingSubscriber::new_with_handler(
			Box::new(handler),
			"test_target"
		);
		(test_subscriber, spans, events)
	}

	#[test]
	fn test_span() {
		let (sub, spans, events) = setup_subscriber();
		let _sub_guard = tracing::subscriber::set_default(sub);
		let span = tracing::info_span!(target: "test_target", "test_span1");
		assert_eq!(spans.lock().len(), 0);
		assert_eq!(events.lock().len(), 0);
		let _guard = span.enter();
		assert_eq!(spans.lock().len(), 0);
		assert_eq!(events.lock().len(), 0);
		drop(_guard);
		drop(span);
		assert_eq!(spans.lock().len(), 1);
		assert_eq!(events.lock().len(), 0);
		let sd = spans.lock().remove(0);
		assert_eq!(sd.name, "test_span1");
		assert_eq!(sd.target, "test_target");
		let time: u128 = sd.overall_time.as_nanos();
		assert!(time > 0);
	}

	#[test]
	fn test_span_parent_id() {
		let (sub, spans, _events) = setup_subscriber();
		let _sub_guard = tracing::subscriber::set_default(sub);
		let span1 = tracing::info_span!(target: "test_target", "test_span1");
		let _guard1 = span1.enter();
		let span2 = tracing::info_span!(target: "test_target", "test_span2");
		let _guard2 = span2.enter();
		drop(_guard2);
		drop(span2);
		let sd2 = spans.lock().remove(0);
		drop(_guard1);
		drop(span1);
		let sd1 = spans.lock().remove(0);
		assert_eq!(sd1.id, sd2.parent_id.unwrap())
	}

	#[test]
	fn test_event() {
		let (sub, _spans, events) = setup_subscriber();
		let _sub_guard = tracing::subscriber::set_default(sub);
		tracing::event!(target: "test_target", tracing::Level::INFO, "test_event");
		let mut te1 = events.lock().remove(0);
		assert_eq!(te1.visitor.0.remove(&"message".to_owned()).unwrap(), "test_event".to_owned());
	}

	#[test]
	fn test_event_parent_id() {
		let (sub, spans, events) = setup_subscriber();
		let _sub_guard = tracing::subscriber::set_default(sub);

		// enter span
		let span1 = tracing::info_span!(target: "test_target", "test_span1");
		let _guard1 = span1.enter();

		// emit event
		tracing::event!(target: "test_target", tracing::Level::INFO, "test_event");

		//exit span
		drop(_guard1);
		drop(span1);

		// check span is not emitted separately
		assert!(events.lock().is_empty());

		let sd1 = spans.lock().remove(0);

		// Check span contains the event
		assert_eq!(sd1.events.len(), 1);
	}
}