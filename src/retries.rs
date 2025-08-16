use crate::*;
use alloc::boxed::Box;
use alloc::collections::LinkedList;
use core::cell::{LazyCell, RefCell};
use critical_section::Mutex;
use esp_hal::delay;
use esp_wifi_sys::include::*;

struct Msg {
    addr: Addr,
    data: Box<[u8]>,
    attempts: u8,
}

type List = LinkedList<Msg>;

static QUEUE: Mutex<LazyCell<RefCell<List>>> =
    Mutex::new(LazyCell::new(|| RefCell::new(List::new())));

pub fn start() {
    _ = unsafe { esp_now_register_send_cb(Some(send_cb)) };
}

pub fn stop() {
    _ = unsafe { esp_now_register_send_cb(None) };
    critical_section::with(|cs| {
        let queue = QUEUE.borrow(cs);
        let mut queue = queue.borrow_mut();
        queue.clear();
    });
}

pub fn send(addr: Addr, data: &[u8]) -> i32 {
    while pending(addr) {
        delay::Delay::new().delay_micros(5);
    }
    let code = unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) };
    if code == 0 {
        critical_section::with(|cs| {
            let queue = QUEUE.borrow(cs);
            let mut queue = queue.borrow_mut();
            queue.push_back(Msg {
                addr,
                data: data.into(),
                attempts: 0,
            });
        });
    }
    code
}

fn confirm(addr: Addr) {
    critical_section::with(|cs| {
        let queue = QUEUE.borrow(cs);
        let mut queue = queue.borrow_mut();
        queue.retain(|item| addr != item.addr);
    });
}

fn pending(addr: Addr) -> bool {
    critical_section::with(|cs| {
        let queue = QUEUE.borrow(cs);
        let queue = queue.borrow();
        queue.iter().any(|item| addr == item.addr)
    })
}

fn retry(addr: Addr) {
    critical_section::with(|cs| {
        let queue = QUEUE.borrow(cs);
        let mut queue = queue.borrow_mut();
        let mut maybe = queue.iter_mut().find(|item| addr == item.addr);
        let Some(msg) = &mut maybe else {
            return;
        };
        msg.attempts += 1;
        if msg.attempts >= 3 {
            queue.retain(|item| addr != item.addr);
        } else {
            let data = &msg.data;
            // TODO: move it outside CS.
            unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) };
        }
    })
}

unsafe extern "C" fn send_cb(addr: *const u8, status: esp_now_send_status_t) {
    let is_ok = status == esp_now_send_status_t_ESP_NOW_SEND_SUCCESS;
    let addr = core::slice::from_raw_parts(addr, 6);
    let addr: Addr = addr.try_into().unwrap();
    if is_ok {
        confirm(addr);
    } else {
        retry(addr);
    }
}
