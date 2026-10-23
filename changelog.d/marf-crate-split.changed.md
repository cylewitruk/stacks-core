Separate the MARF engine, proof transport and pruning into domain packages behind the Stacks facade.
SQLite remains in core.
Native SQL access is explicit, legacy formats remain supported, and invalid persisted ancestry padding reports corruption.
