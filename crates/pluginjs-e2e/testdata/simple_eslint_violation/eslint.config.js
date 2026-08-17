export default [
  {
    rules: {
      // "warn", not "error" — a warning-severity finding is exactly what
      // proves the `--max-warnings 0` fix (see driver_lint.rs's
      // `deny_warnings_args`): eslint already exits nonzero on an "error"
      // finding with no extra flag, so that alone wouldn't exercise it.
      "no-debugger": "warn",
    },
  },
];
