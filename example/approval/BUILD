# Approval gate examples.
#
# A target may declare `approval` to require an explicit decision before it runs:
#   - `approval = True`                                  bare gate, no notice
#   - `approval = {"required": True, "notice": [grp]}`   gate + show input groups
#
# In the interactive TUI the prompt renders on the live view ([y] approve /
# [n] reject / enter to read the notice). In non-TUI mode the notice is logged
# and the target runs only with `--auto-approve`:
#
#   heph run //approval:deploy_db --auto-approve
#   heph run //approval:wipe_staging --auto-approve
#
# Both gated targets set `cache = False` so the approval is asked every run — a
# deploy must never be skipped just because its inputs are unchanged.

# Produces the human-readable migration plan shown as the deploy's notice. In a
# real setup this would be `terraform plan`, `atlas migrate diff`, a rendered
# k8s diff, etc. — any reviewable artifact a human should read before approving.
plan = target(
    name = "migration_plan",
    driver = "bash",
    out = "plan.txt",
    run = """
cat > $OUT <<'PLAN'
=== Database migration plan: orders-service @ prod ===

  ALTER TABLE orders ADD COLUMN fulfilled_at timestamptz;
  CREATE INDEX CONCURRENTLY idx_orders_fulfilled_at ON orders (fulfilled_at);
  UPDATE orders SET fulfilled_at = shipped_at WHERE status = 'shipped';
  DROP COLUMN orders.legacy_shipped_flag;          -- IRREVERSIBLE

Estimated rows rewritten : 4,812,003
Estimated lock time      : ~0s (CONCURRENTLY)
Estimated duration       : 6-9 min
Rollback                 : restore from snapshot prod-2026-06-28-0300

Review the statements above before approving. This runs against PRODUCTION.
PLAN
""",
)

# Gated deploy with a long notice: approving prints the migration plan first.
# The dep group `plan` is exposed to `run` as $SRC_PLAN (path to plan.txt).
target(
    name = "deploy_db",
    driver = "bash",
    labels = ["deploy"],
    approval = {"required": True, "notice": ["plan"]},
    deps = {"plan": plan},
    cache = False,
    run = "echo 'applying migration to prod...'; cat $SRC_PLAN; echo 'done.'",
)

# Gated destructive action with no notice — just a bare confirmation prompt.
target(
    name = "wipe_staging",
    driver = "bash",
    labels = ["deploy"],
    approval = True,
    cache = False,
    run = "echo 'wiping staging environment...'; echo 'staging reset complete.'",
)
