use anyhow::{Context, Result};
use esp_backtrace as _;
use esp_hal::{uart::Uart, Blocking};
use esp_println::println;
use firefly_types::{spi::*, Encode};

/// Serialize response and write it into UART.
pub fn send_resp(uart: &mut Uart<'_, Blocking>, buf: &mut [u8], resp: Response<'_>) -> Result<()> {
    if resp == Response::NetSent {
        return Ok(());
    }
    let (head, tail) = buf.split_at_mut(1);
    let buf = resp.encode_buf(tail).context("encode response")?;
    let Ok(size) = u8::try_from(buf.len()) else {
        // The payload is too big. The only Response that can, in theory, be big
        // is NetIncoming. So we can assume that it's a message receiving error.
        // But just in case, we want to be sure not to fall into an infinite recursion.
        if !matches!(resp, Response::Error(_)) {
            println!("error: response is too big");
            let resp = Response::Error("response is too big");
            send_resp(uart, buf, resp)?;
        }
        return Ok(());
    };
    head[0] = size;
    uart.write(head).context("write size")?;
    uart.write(buf).context("write response")?;
    Ok(())
}
