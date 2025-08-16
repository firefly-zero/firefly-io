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

type List = LinkedList<Msg>;

static PENDING: Mutex<RefCell<List>> = Mutex::new(RefCell::new(List::new()));

pub fn start() -> Result<(), EspNowError> {
    let code = unsafe { esp_now_register_send_cb(Some(send_cb)) };
    parse_error_code(code)
}

pub fn stop() -> Result<(), EspNowError> {
    let code = unsafe { esp_now_register_send_cb(None) };
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let mut pending = pending.borrow_mut();
        pending.clear();
    });
    parse_error_code(code)
}

pub fn send(addr: Addr, data: &[u8]) -> Result<(), EspNowError> {
    while is_pending(addr) {
        delay::Delay::new().delay_micros(5);
    }
    let code = unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) };
    if code == 0 {
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
    parse_error_code(code)
}

#[must_use]
pub fn get_status(addr: Addr) -> SendStatus {
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let pending = pending.borrow_mut();
        let maybe_msg = pending.iter().find(|msg| msg.addr == addr);
        let Some(msg) = maybe_msg else {
            return SendStatus::Delivered(0);
        };
        SendStatus::Sending(msg.attempts)
    })
}

fn confirm(addr: Addr) {
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let mut pending = pending.borrow_mut();
        pending.retain(|msg| msg.addr != addr);
    });
}

fn is_pending(addr: Addr) -> bool {
    critical_section::with(|cs| {
        let pending = PENDING.borrow(cs);
        let pending = pending.borrow();
        pending.iter().any(|msg| msg.addr == addr)
    })
}

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
            pending.retain(|item| addr != item.addr);
            0
        } else {
            let data = &msg.data;
            // TODO: move it outside CS.
            unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) }
        }
    });
    parse_error_code(code)
}

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

fn parse_error_code(code: core::ffi::c_int) -> Result<(), EspNowError> {
    if code == 0 {
        Ok(())
    } else {
        #[allow(clippy::cast_sign_loss)]
        let err = esp_wifi::esp_now::Error::from_code(code as u32);
        Err(EspNowError::Error(err))
    }
}
