#include "textflag.h"

// Define the function entry.
// NOSPLIT avoids stack-split prologue.
// $0 says we don’t need any extra stack beyond args.
TEXT ·Add(SB), NOSPLIT, $0
    // load x, y from the frame
    MOVD    x+0(FP), R0      // R0 ← x
    MOVD    y+8(FP), R1      // R1 ← y
    // R0 = R0 + R1
    //   ADD <src2>, <src1>, <dst>  sets dst = src1 + src2
    ADD     R1, R0, R0
    // store result back to ret+16(FP)
    MOVD    R0, ret+16(FP)
    RET
