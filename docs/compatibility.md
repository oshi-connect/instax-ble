# Instax image transfer

The driver uses the paper dimensions and JPEG byte limit returned by
`IMAGE_SUPPORT`. If the byte limit is absent or zero, it uses 105 KiB.
JPEG quality is reduced to fit; pixel dimensions stay at the printer size.
`DOWNLOAD_START` supplies the chunk size. Older replies without this field
use the model default. Invalid chunk sizes stop the transfer.

Some firmware advertises more bytes than it accepts. Only an explicit
`DOWNLOAD_START` rejection with status `0x81` permits one retry at 105 KiB,
and only if the rejected JPEG was larger. Timeouts, later transfer failures,
and print-command failures never trigger this retry.

## Mini Link+ hardware check — 2026-09-20

- Reported dimensions: 600 × 800.
- Reported JPEG limit: 307,200 bytes.
- Rejected a 303,539-byte JPEG at START with `0x81`.
- Accepted the same input encoded to 102,449 bytes in 114 chunks of 900 bytes.
- Accepted `DOWNLOAD_END`.
- Refused cancellation after END with `0x82`. A send-only diagnostic reports
  this and disconnects; it does not claim the printer's image buffer was
  cleared.
- Film count before and after: 3. No `PRINT_IMAGE` command was sent.

The automatic fallback passed on this Link+. Other models have unit coverage
for sizing and protocol handling, but were not physically tested in this check.
Physical print output was not tested.

## Send without printing

A caller can upload and validate an image without printing it, to exercise the
same encode, handshake, and fallback as a real print. The transport method
`InstaxBle::send_only` sends START, DATA, END, and a cancellation request. It
never sends PRINT_IMAGE. It then leaves the film count unchanged.

On macOS, run the host from a signed app bundle with a Bluetooth usage
description. Closing the Fujifilm app first prevents a competing Bluetooth
connection.
