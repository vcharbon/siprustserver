# SIP Rust server

Project is ultra early, not in production, do not worry about upgrade compatibility when designing solutions
Ongoing port of https://github.com/vcharbon/sipjsserver to rust to improve perfs.

Read the [strategy](./docs/MIGRATION_STRATEGY.md), it is currently beta and will be enriched with consolidated decision 

## Overall Action when migrating a module

For each Layer to be migrated, [update migration](./MIGRATION_STATUS.md) file with the exact release used as a source
Port the Layer interface an implementation, the test implementation, including the property test and Layer comparison
Port an pass all test of the given layer. Provide a full list of un-ported test with precise justification for the case where it is not.

