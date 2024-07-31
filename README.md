# Stackable OPA Bundle Builder

This is a helper utility for the Stackable Operator for [OPA](https://www.openpolicyagent.org/).

## DEPRECATED

This functionality has been moved into the [Stackable Operator for OPA](https://github.com/stackabletech/opa-operator/tree/main/rust/bundle-builder). It is still used by some supported versions of the
Stackable Data Platform, but will not be supported anymore outside of that context.

## Purpose

The sole purpose of the OPA Bundle Builder is to convert user created `ConfigMaps` containing [Rego](https://www.openpolicyagent.org/docs/latest/policy-language/) rules into bundles (`tar.gz` files) and make them available as an HTTP endpoint. The Bundle Builder runs in a side container of the OPA `Pod` managed by the [Stackable Operator for OPA](https://docs.stackable.tech/opa/nightly/index.html) as a simple HTTP server that OPA is querying regularly (every 20 to 30 seconds) for updates.

This allows users to add, update or delete Rego rules on-the-fly in Kubernetes.

## Notes

NOTE: Kubernetes limits the size of `ConfigMap`s to 1MB. Users have to take this limit into consideration when manging policy rules.

Only ConfigMaps labeled with `opa.stackable.tech/bundle: "true"` are considered by the builder when updating bundles. The name of
the `data` entries in the `ConfigMap` are used as file names when storing the rules in the bundle.

NOTE: Currently it is the user's responsibility to make sure these names do not collide (as they will override each other).

