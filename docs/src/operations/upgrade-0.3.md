# Upgrading to 0.3

This release replaces most built-in processes with DSL functions, adds first-class string interpolation, and introduces `limpidctl check`. Configuration written for 0.2 will not load unchanged — all of the breaking items below are simple mechanical rewrites.

## New features

- **`strftime(value, format[, tz])`** for timestamp formatting. See [Timestamp formatting](../processing/functions.md#timestamp-formatting).
- **`+` concatenates strings** when either operand is a string; stays as numeric addition otherwise.
