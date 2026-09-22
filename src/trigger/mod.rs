//! A trigger runs a check and records its result. Scheduling does not read
//! Presented receipts or historical Habit completions. Orient alone delivers
//! notifications; the synchronous event caller receives its own verdict.

pub mod runner;
pub mod model;
pub mod operations;
pub mod cli;
pub mod dispatch;
pub mod repository;
