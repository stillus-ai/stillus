// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Application operations shared by native views and direct Rust tools.
pub(crate) mod ai;
pub(crate) mod journal;
pub(crate) mod persistence;
pub(crate) mod rss;
pub(crate) mod search;
pub(crate) mod security;
pub(crate) mod settings;
#[allow(
    dead_code,
    reason = "Direct-call tool registry is prepared before the assistant UI."
)]
pub(crate) mod tools;
pub(crate) mod updates;
pub(crate) mod workspace;

pub(crate) mod runtime;
pub(crate) use runtime::{Application, ApplicationEvent};

#[allow(
    dead_code,
    reason = "Typed actions are exposed to the direct Rust dispatcher."
)]
pub(crate) mod actions;

pub(crate) mod global;

#[allow(
    dead_code,
    reason = "Typed dispatcher is shared with direct Rust tool calls."
)]
pub(crate) mod api;

pub(crate) mod catalog;

pub(crate) mod preferences;

#[cfg(test)]
mod api_tests;

pub(crate) mod chat;
