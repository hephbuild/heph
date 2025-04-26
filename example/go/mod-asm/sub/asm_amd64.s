#include "textflag.h"

// TEXT ·Add(SB), NOSPLIT, $0
//   ·Add  the symbol
//   NOSPLIT  omit stack-split check (safe: tiny function)
//   $0    no additional stack frame needed
TEXT ·Add(SB), NOSPLIT, $0
    // load x, y from 0(FP), 8(FP)
    MOVQ    x+0(FP), AX
    MOVQ    y+8(FP), BX
    // AX = AX + BX
    ADDQ    BX, AX
    // store result to ret+16(FP)
    MOVQ    AX, ret+16(FP)
    RET
