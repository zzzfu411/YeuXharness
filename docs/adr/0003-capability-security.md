# ADR 0003: Capabilities, approvals and sandboxing

Status: accepted

Tools implement `prepare(args)` before execution. Preparation normalizes input,
derives a conservative requested `EffectSet` and returns a digest-bound token.
Policy and approval select a subset of that request; the operating-system
sandbox then enforces the resulting upper bound.

`observe`, `build` and `operate` are named presets rather than alternate
execution paths. Approval can never override a host or user deny. Project
configuration may only reduce capabilities until the project is trusted.

