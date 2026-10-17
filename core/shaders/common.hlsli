// Shared by every kernel: up to 32 constants, then a table of one input (t0) and one output (u0).
// The root signature is compiled into each shader, so the engine needs no separate description.
#define ROOT_SIGNATURE \
    "RootConstants(num32BitConstants=32, b0)," \
    "DescriptorTable(SRV(t0), UAV(u0))," \
    "StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR," \
    " addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP)"
