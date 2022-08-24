module mod-with-replace-local

go 1.18

replace mod-simple => ../mod-simple

require mod-simple v0.0.0-00010101000000-000000000000
