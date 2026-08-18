mod custom;
mod events;
#[cfg(feature = "diagnostics-test-support")]
mod fragment_test_support;
mod install;
mod sink;

pub(crate) use install::install;
