use core::cell::{LazyCell, RefCell};

use crate::*;
use alloc::collections::VecDeque;
use alloc::{boxed::Box, vec::Vec};
use critical_section::Mutex;
use esp_wifi_sys::include::*;
use portable_atomic::{AtomicBool, AtomicU8, Ordering};

static ESP_NOW_SEND_CB_INVOKED: AtomicBool = AtomicBool::new(false);
static ESP_NOW_SEND_STATUS: AtomicBool = AtomicBool::new(true);

struct Msg {
    addr: Addr,
    data: Box<[u8]>,
    attempts: u8,
}

type Queue = VecDeque<Msg>;

static QUEUE: Mutex<LazyCell<RefCell<Queue>>> =
    Mutex::new(LazyCell::new(|| RefCell::new(Queue::new())));

pub fn send(addr: Addr, data: &[u8]) -> i32 {
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
        let maybe = queue.iter().enumerate().find(|(_, item)| addr == item.addr);
        let Some((i, _)) = maybe else {
            return;
        };
        queue.remove(i);
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
        let data = &msg.data;
        // TODO: mave it outside CS.
        unsafe { esp_now_send(addr.as_ptr(), data.as_ptr(), data.len()) };
    })
}

pub unsafe extern "C" fn send_cb(_mac_addr: *const u8, status: esp_now_send_status_t) {
    critical_section::with(|_| {
        let is_ok = status == esp_now_send_status_t_ESP_NOW_SEND_SUCCESS;
        ESP_NOW_SEND_STATUS.store(is_ok, Ordering::Relaxed);
        ESP_NOW_SEND_CB_INVOKED.store(true, Ordering::Release);
    })
}
