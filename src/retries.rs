use crate::*;
use alloc::boxed::Box;
use alloc::collections::LinkedList;
use core::cell::RefCell;
use critical_section::Mutex;
use esp_hal::delay;
use esp_wifi::esp_now::EspNowError;
use esp_wifi_sys::include::*;
use firefly_types::spi::SendStatus;

const MAX_RETRIES: u8 = 15;

struct Msg {
    addr: Addr,
    data: Box<[u8]>,
    attempts: u8,
}

struct State {
    addr: Addr,
    status: SendStatus,
}

type Msgs = LinkedList<Msg>;
type States = LinkedList<State>;

static PENDING: Mutex<RefCell<Msgs>> = Mutex::new(RefCell::new(Msgs::new()));
static STATES: Mutex<RefCell<States>> = Mutex::new(RefCell::new(States::new()));

/// Register the send callback.
pub fn start() -> Result<(), EspNowError> {
    let code = unsafe { esp_now_register_send_cb(Some(send_cb)) };
    parse_error_code(code)
}

/// Unregister the send callback, clear pending messages and delivery states.
pub fn stop() -> Result<(), EspNowError> {
    let code = unsafe { esp_now_register_send_cb(None) };
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let mut pending = pending.borrow_mut();
        pending.clear();
    });
    parse_error_code(code)
}

/// Send a message with retries.
///
/// If there is already a pending message for the given peer,
/// blocks until that message is delivered.
pub fn send(addr: Addr, data: &[u8]) -> Result<(), EspNowError> {
    while is_pending(addr) {
        delay::Delay::new().delay_micros(5);
    }
    set_status(addr, SendStatus::Sending(0));
    let code = unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) };
    if code == 0 {
        store_pending(addr, data);
    }
    parse_error_code(code)
}

fn store_pending(addr: Addr, data: &[u8]) {
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let mut pending = pending.borrow_mut();
        pending.push_back(Msg {
            addr,
            data: data.into(),
            attempts: 0,
        });
    });
}

/// Get the delivery state of the latest message for the given peer.
#[must_use]
pub fn get_status(addr: Addr) -> SendStatus {
    critical_section::with(|cs| {
        let states = STATES.borrow(cs);
        let states = states.borrow();
        let maybe_state = states.iter().find(|state| state.addr == addr);
        let Some(state) = maybe_state else {
            return SendStatus::Empty;
        };
        state.status
    })
}

/// Mark the latest message for the peer as delivered.
fn confirm(addr: Addr) {
    set_status(addr, SendStatus::Delivered(0));
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let mut pending = pending.borrow_mut();
        pending.retain(|msg| msg.addr != addr);
    });
}

/// Check if there is already a message for the peer that is sent and waiting for ack.
fn is_pending(addr: Addr) -> bool {
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let pending = pending.borrow();
        pending.iter().any(|msg| msg.addr == addr)
    })
}

/// Try re-delivering the latest message for the peer.
fn retry(addr: Addr) -> Result<(), EspNowError> {
    let code = critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let mut pending = pending.borrow_mut();
        let mut maybe = pending.iter_mut().find(|msg| msg.addr == addr);
        let Some(msg) = &mut maybe else {
            return 0;
        };
        msg.attempts += 1;
        if msg.attempts >= MAX_RETRIES {
            set_status(addr, SendStatus::Failed);
            pending.retain(|item| addr != item.addr);
            0
        } else {
            let data = &msg.data;
            set_status(addr, SendStatus::Sending(msg.attempts));
            // TODO: move it outside CS.
            unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) }
        }
    });
    parse_error_code(code)
}

/// The callback triggered by esp-now C intrisics on ack/nak of the message.
unsafe extern "C" fn send_cb(addr: *const u8, status: esp_now_send_status_t) {
    let is_ok = status == esp_now_send_status_t_ESP_NOW_SEND_SUCCESS;
    let addr = core::slice::from_raw_parts(addr, 6);
    let addr: Addr = addr.try_into().unwrap();
    if is_ok {
        confirm(addr);
    } else {
        _ = retry(addr);
    }
}

// Store the delivery status of the latest message for the peer.
fn set_status(addr: Addr, send_status: SendStatus) {
    critical_section::with(|cs| {
        let states = STATES.borrow(cs);
        let mut states = states.borrow_mut();
        states.retain(|state| state.addr != addr);
        states.push_back(State {
            addr,
            status: send_status,
        });
    });
}

/// Convert error code returned by esp-now C library into a Rust-friendly error.
fn parse_error_code(code: core::ffi::c_int) -> Result<(), EspNowError> {
    if code == 0 {
        Ok(())
    } else {
        #[allow(clippy::cast_sign_loss)]
        let err = esp_wifi::esp_now::Error::from_code(code as u32);
        Err(EspNowError::Error(err))
    }
}
