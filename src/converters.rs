use std::collections::BTreeMap;

use eyre::Context;
use sentry_backtrace::current_stacktrace;
use sentry_core::protocol::{Event, Exception};
use sentry_core::Breadcrumb;
use tracing::field::Field;

use crate::TracingIntegration;

fn convert_tracing_level(level: &tracing::Level) -> sentry_core::Level {
    match level {
        &tracing::Level::ERROR => sentry_core::Level::Error,
        &tracing::Level::WARN => sentry_core::Level::Warning,
        &tracing::Level::INFO => sentry_core::Level::Info,
        &tracing::Level::DEBUG | &tracing::Level::TRACE => sentry_core::Level::Debug,
    }
}

struct FieldVisitorConfig {
    /// Strip ansi escape sequences from string values, and formatted error messages.
    pub strip_ansi_escapes: bool,
}

impl Default for FieldVisitorConfig {
    fn default() -> Self {
        Self {
            strip_ansi_escapes: false,
        }
    }
}

impl From<&TracingIntegration> for FieldVisitorConfig {
    fn from(integration: &TracingIntegration) -> Self {
        Self {
            strip_ansi_escapes: integration.strip_ansi_escapes,
        }
    }
}

#[derive(Default)]
struct FieldVisitorResult {
    pub display_values: Vec<String>,
    pub json_values: BTreeMap<String, serde_json::Value>,
    pub log_target: Option<String>,
}

impl FieldVisitorResult {
    fn message(&self) -> String {
        self.display_values.join("\n")
    }
}

#[derive(Default)]
struct FieldVisitor {
    config: FieldVisitorConfig,
    result: FieldVisitorResult,
}

impl FieldVisitor {
    fn visit_event(event: &tracing::Event<'_>, config: FieldVisitorConfig) -> FieldVisitorResult {
        let mut visitor = Self {
            config,
            ..Self::default()
        };

        event.record(&mut visitor);
        visitor.result
    }

    fn record_json_value<S: serde::Serialize>(&mut self, field: &Field, value: &S) {
        match serde_json::to_value(value) {
            Ok(json_value) => {
                self.result.json_values.insert(field.name().to_owned(), json_value);
            },
            Err(error) => {
                let error = eyre::eyre!("Error while serializing the \"{}\" field to json: {}", field.name(), error);
                tracing::error!(error = ?error)
            },
        }
    }

    fn record_message(&mut self, field: &Field, value: &str) {
        self.result.display_values.push(format!("{}={}", field, value));
    }
}

/// Strips ansi color escape codes from string, or returns the
/// original string if there was problem performing the strip.
pub fn strip_ansi_codes_from_string(string: &str) -> String {
    if let Ok(stripped_bytes) = strip_ansi_escapes::strip(string.as_bytes()) {
        if let Ok(stripped_string) = std::str::from_utf8(&stripped_bytes) {
            return stripped_string.to_owned();
        }
    }

    string.to_owned()
}

impl tracing::field::Visit for FieldVisitor {
    /// Visit a signed 64-bit integer value.
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_json_value(field, &value);
        self.record_message(field, &format!("{:?}", value));
    }

    /// Visit an unsigned 64-bit integer value.
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_json_value(field, &value);
        self.record_message(field, &&format!("{:?}", value));
    }

    /// Visit a boolean value.
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_json_value(field, &value);
        self.record_message(field, &&format!("{:?}", value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let value = if self.config.strip_ansi_escapes {
            strip_ansi_codes_from_string(&value)
        } else {
            value.to_owned()
        };

        if field.name() == "log.target" {
            self.result.log_target = Some(value.clone());
        }

        self.record_json_value(field, &value);
        self.record_message(field, &value);
    }
    
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let formatted_value = format!("{:?}", value);
        let message_string = if self.config.strip_ansi_escapes {
            strip_ansi_codes_from_string(&formatted_value)
        } else {
            formatted_value
        };

        self.record_json_value(field, &message_string);
        self.record_message(field, &message_string);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let formatted_value = format!("{:?}", value);
        let message_string = if self.config.strip_ansi_escapes {
            strip_ansi_codes_from_string(&formatted_value)
        } else {
            formatted_value
        };

        self.record_json_value(field, &message_string);
        self.record_message(field, &message_string);
    }
}

/// Creates a breadcrumb from a given tracing event.
pub fn breadcrumb_from_event<S: tracing::Subscriber>(event: &tracing::Event<'_>, context: &tracing_subscriber::layer::Context<'_, S>, integration: &TracingIntegration) -> Breadcrumb {
    let visitor_result = FieldVisitor::visit_event(event, integration.into());

    Breadcrumb {
        ty: "log".into(),
        level: convert_tracing_level(event.metadata().level()),
        category: Some(event.metadata().target().into()),
        message: Some(visitor_result.message()),
        data: visitor_result.json_values,
        ..Default::default()
    }
}

/// Creates an event from a given log record.
///
/// If `with_stacktrace` is set to `true` then a stacktrace is attached
/// from the current frame.
pub fn convert_tracing_event<S: tracing::Subscriber>(event: &tracing::Event<'_>, context: &tracing_subscriber::layer::Context<'_, S>, integration: &TracingIntegration) -> Event<'static> {
    let visitor_result = FieldVisitor::visit_event(event, integration.into());

    // Special support for log.target reported by tracing-log
    // TODO: add support for rendering event `type` field
    let exception_type = match &visitor_result.log_target {
        Some(log_target) => {
            format!("[{}] log event", log_target)
        }
        None => {
            format!("[{}] tracing event", event.metadata().target())
        }
    };

    Event {
        logger: Some("sentry-tracing".into()),
        level: convert_tracing_level(event.metadata().level()),
        exception: vec![Exception {
            ty: exception_type,
            value: Some(visitor_result.message()),
            stacktrace: if integration.attach_stacktraces {
                current_stacktrace()
            } else {
                None
            },
            module: event.metadata().module_path().map(|p| p.to_owned()),
            ..Default::default()
        }]
        .into(),
        ..Default::default()
    }
}
