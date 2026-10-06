# Original RAR5 fixtures

The Rust writer constructs headers from the published RAR5 structure, using an
independent bitwise CRC. `original.hex` contains only original ASCII media fixture
data and one stored `original.txt` entry. It was written once as fixture-data
editing with Python standard-library integer encoding and `binascii.crc32`, using
the published structures. No third-party source or binary was copied. Python and
binascii are not Mynou runtime or build dependencies. CI alone executes tests.

Protocol source: https://www.rarlab.com/technote.htm
