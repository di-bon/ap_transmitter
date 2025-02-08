# Transmitter
This repo contains the code for the Transmitter for the Advanced Programming course help at the University of Trento in the academic year 2024/2025.

## Description
The Transmitter will handle the transmission of Packets to the neighboring drones.
The Transmitter can be used by:
- Listener, which will send ```transmitter::PacketCommand```s to 
communicate which action to perform
- Logic (either a Server or a Client), which will send ```Message```s

To transmit a ```Message```, the transmitter will create a new thread 
running the ```TransmissionHandler```, which will first find a ```SourceRoutingHeader```,
then will fragment the message and finally will send all the fragments. 
It will also handle the re-transmissions if a ```NackType::Dropped``` 
packet is received.

To transmit a ```Packet```, the transmitter will create a new thread 
running the ```SinglePacketTransmissionHandler```, which is a simpler
version of ```TransmissionHandler``` that only sends one 
```Packet```, after finding an appropriate ```SourceRoutingHeader```.

## Usage
To use the Transmitter, add
```toml
ap_transmitter = { git = "https://github.com/di-bon/ap_transmitter.git" }
```
to your Cargo.toml file.

Then, import it in your project files using
```rust
use ap_transmitter::Transmitter;
```

To create a new Transmitter, use the constructor ```Transmitter::new```.

To make the Transmitter work, call ```Transmitter::run()```.

## Panics
See the documentation for each function.
