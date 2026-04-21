# Agents Notes

Follow these principles when working in this repository:

1. When optimizing performance or refactoring execution paths, do not keep the previous slow path as a compatibility branch. Once the new primary path is decided, replace the old implementation directly to avoid redundant code, higher maintenance cost, and behavioral drift.
2. Do not accumulate excessive fallback logic. Keep fallback branches only when there is a clear external constraint; otherwise prefer fewer branches and a single clear, predictable execution path so future maintainers are not misled.
