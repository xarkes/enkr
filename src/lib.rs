//! enkr library: note model + E2EE sync engine + the application itself.
//!
//! The binary (`main.rs`) is a thin GUI entry point; everything else lives
//! here so the test harness can drive the real app, the sync engine, and the
//! note model in-process.

pub mod app;
pub mod note;
pub mod search;
#[cfg(feature = "testkit")]
pub mod showcase;
pub mod sync;
#[cfg(feature = "cdp")]
pub mod testkit_support;

/// Generates `<name>::native` and (feature = "cdp") `<name>::cdp` `#[test]`s
/// from a scenario fn `name(&mut impl mae::testkit::UiDriver)`, a harness
/// size, and a state constructor expression. `render` must already be in
/// scope at the call site (`use super::*` picks it up).
///
/// The size applies to *both* backends: the browser one emulates that
/// viewport (`CdpDriver::set_viewport`). It used to be handed to the native
/// harness alone while the browser ran at whatever size Chrome launched at,
/// which meant a scenario about a size-dependent layout could not be written
/// against the DOM backend at all.
#[macro_export]
macro_rules! driver_test {
    ($name:ident, $w:expr, $h:expr, $state:expr) => {
        mod $name {
            use super::*;

            #[test]
            fn native() {
                let mut state = $state;
                let mut driver = mae::testkit::NativeDriver::new($w, $h, move |ui| {
                    state.set_repaint_waker(ui.repaint_waker());
                    render(ui, &mut state);
                });
                super::$name(&mut driver);
            }

            #[cfg(feature = "cdp")]
            #[test]
            fn cdp() {
                let mut driver = $crate::testkit_support::launch_test_harness_sized($w, $h);
                super::$name(&mut driver);
            }
        }
    };
}
