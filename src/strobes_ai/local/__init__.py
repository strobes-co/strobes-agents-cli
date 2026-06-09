"""Local execution primitives — the user's machine acts as the sandbox.

These modules are shared by:
  * the standalone bridge daemons (``strobes_ai.bridges.*``), which the cloud
    sandbox/browser tools route to, and
  * the in-band CLI_LOCAL path in the pulse client, which handles
    ``tool.local_execute`` events directly.
"""
