# Original DEFLATE fixture data

`fixed.hex` and `dynamic.hex` encode only `original_payload()` in the adjacent
fixture module. They were generated once with Python's standard-library zlib
encoder using raw DEFLATE (`wbits=-15`), level 9, memory level 9, and respectively
`Z_FIXED` and `Z_DEFAULT_STRATEGY`. This is synthetic test data, not copied
implementation code or a runtime/build dependency. CI decodes these independent
streams, compares every original byte, and checks independently calculated CRC
and SHA-256. No project executable or local validation is used in generation.

`demo.hex` similarly encodes only the original `examples/demo.mp4` using raw
DEFLATE, level 9, memory level 9 and the default strategy. It is fixture data for
native ZIP library admission and introduces no runtime or build dependency.
