# firefly-io

Firemware for the second chip on Firefly Zero. At the moment, it handles multiplayer, buttons, and touchpad. It communicates with the main chip by passing binary messages over UART.

## Flashing

1. [Install espup](https://github.com/esp-rs/espup)
1. [Install task](https://taskfile.dev/)
1. `espup install`
1. `. ~/export-esp.sh`
1. Connect to the right chip on the device.
1. `task flash -- --port /dev/ttyACM0`
