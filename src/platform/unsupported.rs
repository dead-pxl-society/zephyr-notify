//! Lets the crate compile on targets such as mobile, where `Notifier::build` then fails.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::notification::{Capabilities, Notification, Permission};
use crate::notifier::{AppConfig, BackendApi, EventSink, Present};

pub(crate) enum UnsupportedBackend {}

impl BackendApi for UnsupportedBackend {
    fn new(_: &AppConfig, _: EventSink) -> Result<Self> {
        Err(Error::Unsupported)
    }
    fn show(&self, _: &Notification, _: Present) -> Result<()> {
        match *self {}
    }
    fn close(&self, _: &str) -> Result<()> {
        match *self {}
    }
    fn close_all(&self) -> Result<()> {
        match *self {}
    }
    fn capabilities(&self) -> Capabilities {
        match *self {}
    }
    fn permission(&self) -> Result<Permission> {
        match *self {}
    }
    fn request_permission(&self) -> Result<Permission> {
        match *self {}
    }
    fn min_progress_interval(&self) -> Duration {
        match *self {}
    }
}
