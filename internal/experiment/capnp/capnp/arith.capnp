using Go = import "/go.capnp";

@0xf454c62f08bc504b;

$Go.package("arith");
$Go.import("arith");

# Declare the Arith capability, which provides multiplication and division.
interface Arith {
	multiply @0 (a :Int64, b :Int64) -> (product :Int64);
	divide   @1 (num :Int64, denom :Int64) -> (quo :Int64, rem :Int64);
}