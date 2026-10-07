//! Progression d’ouverture, envoyée à la fenêtre d’accueil.
//!
//! Le travail long (catalogue zip, index des noms) tourne hors du thread
//! principal. L’accueil ne peut se redessiner que si ce thread reste libre.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Clone, Serialize)]
pub(crate) struct OpenProgress {
    pub gen: u64,
    pub label: String,
    pub ratio: Option<f32>,
    pub detail: String,
    pub done: bool,
}

impl OpenProgress {
    pub(crate) fn finished(gen: u64) -> Self {
        Self {
            gen,
            label: String::new(),
            ratio: None,
            detail: String::new(),
            done: true,
        }
    }
}

struct Inner {
    gen: u64,
    emit: Box<dyn FnMut(OpenProgress) + Send>,
    last: Option<Instant>,
    label: String,
}

#[derive(Clone)]
pub(crate) struct Reporter {
    inner: Arc<Mutex<Inner>>,
}

impl Reporter {
    pub(crate) fn new(gen: u64, emit: impl FnMut(OpenProgress) + Send + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                gen,
                emit: Box::new(emit),
                last: None,
                label: String::new(),
            })),
        }
    }

    #[cfg(test)]
    pub(crate) fn silent() -> Self {
        Self::new(0, |_| {})
    }

    pub(crate) fn force(&self, label: &str, ratio: Option<f32>, detail: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.last = Some(Instant::now());
        inner.label = label.to_string();
        let event = OpenProgress {
            gen: inner.gen,
            label: inner.label.clone(),
            ratio,
            detail,
            done: false,
        };
        (inner.emit)(event);
    }

    /// Émet au plus toutes les 80 ms, tout de suite quand l’étape change ou se termine.
    pub(crate) fn tick(&self, label: &str, done: u64, total: Option<u64>, detail: String) {
        let ratio = match total {
            Some(0) => Some(1.0),
            Some(total) => Some((done as f32 / total as f32).clamp(0.0, 1.0)),
            None => None,
        };
        let complete = total.is_some_and(|total| done >= total);
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let changed = inner.label != label;
        let due = match inner.last {
            None => true,
            Some(last) => last.elapsed() >= Duration::from_millis(80),
        };
        if !changed && !complete && !due {
            return;
        }
        inner.last = Some(Instant::now());
        inner.label = label.to_string();
        let event = OpenProgress {
            gen: inner.gen,
            label: inner.label.clone(),
            ratio,
            detail,
            done: false,
        };
        (inner.emit)(event);
    }
}

pub(crate) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub(crate) fn notes(n: u64) -> String {
    if n == 1 {
        "1 note".to_string()
    } else {
        format!("{} notes", grouped(n))
    }
}

pub(crate) fn count_span(done: u64, total: u64) -> String {
    format!("{} / {}", grouped(done), grouped(total))
}

pub(crate) fn byte_span(done: u64, total: u64) -> String {
    format!("{} / {}", bytes(done), bytes(total))
}

fn bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let value = n as f64;
    let (scaled, unit) = if value < K {
        return format!("{} octets", grouped(n));
    } else if value < K * K {
        (value / K, "Ko")
    } else if value < K * K * K {
        (value / (K * K), "Mo")
    } else {
        (value / (K * K * K), "Go")
    };
    let mut text = format!("{scaled:.1}").replace('.', ",");
    if let Some(stripped) = text.strip_suffix(",0") {
        text = stripped.to_string();
    }
    format!("{text} {unit}")
}
