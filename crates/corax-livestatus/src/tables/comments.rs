//! Own columns of the `comments` and `downtimes` tables.

use corax_core::{Comment, Downtime, Target};

use super::{getter, Getter, Row};
use crate::value::Value;

fn c<F>(f: F) -> Option<Getter>
where
    F: Fn(&Comment) -> Value + Send + Sync + 'static,
{
    Some(getter(move |_, r| match r {
        Row::Comment(x) => f(x),
        _ => Value::Null,
    }))
}

fn d<F>(f: F) -> Option<Getter>
where
    F: Fn(&Downtime) -> Value + Send + Sync + 'static,
{
    Some(getter(move |_, r| match r {
        Row::Downtime(x) => f(x),
        _ => Value::Null,
    }))
}

pub fn comment_column(name: &str) -> Option<Getter> {
    match name {
        "id" => c(|x| Value::Int(x.id as i64)),
        "author" => c(|x| Value::str(&x.author)),
        "comment" => c(|x| Value::str(&x.text)),
        "entry_time" => c(|x| Value::Int(x.entry_time)),
        "entry_type" => c(|x| Value::Int(x.entry_type as i64)),
        "expire_time" => c(|x| Value::Int(x.expire_time)),
        "expires" => c(|x| Value::bool(x.expires)),
        "persistent" => c(|x| Value::bool(x.persistent)),
        "source" => c(|x| Value::Int(x.source as i64)),
        "is_service" => c(|x| Value::bool(matches!(x.target, Target::Service(_)))),
        // Nagios: HOST_COMMENT = 1, SERVICE_COMMENT = 2
        "type" => c(|x| Value::Int(if matches!(x.target, Target::Service(_)) { 2 } else { 1 })),
        _ => None,
    }
}

pub fn downtime_column(name: &str) -> Option<Getter> {
    match name {
        "id" => d(|x| Value::Int(x.id as i64)),
        "author" => d(|x| Value::str(&x.author)),
        "comment" => d(|x| Value::str(&x.comment)),
        "duration" => d(|x| Value::Int(x.duration)),
        "start_time" => d(|x| Value::Int(x.start_time)),
        "end_time" => d(|x| Value::Int(x.end_time)),
        "entry_time" => d(|x| Value::Int(x.entry_time)),
        "fixed" => d(|x| Value::bool(x.fixed)),
        "is_pending" => d(|x| Value::bool(!x.is_in_effect)),
        "is_service" => d(|x| Value::bool(matches!(x.target, Target::Service(_)))),
        "triggered_by" => d(|x| Value::Int(x.triggered_by as i64)),
        "origin" | "recurring" => d(|_| Value::Int(0)),
        // Nagios: SERVICE_DOWNTIME = 1, HOST_DOWNTIME = 2
        "type" => d(|x| Value::Int(if matches!(x.target, Target::Service(_)) { 1 } else { 2 })),
        _ => None,
    }
}
