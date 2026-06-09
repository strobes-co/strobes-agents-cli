"""Local bridge daemons — expose the user's machine to a remote Strobes agent.

The shell bridge turns this machine into the agent's sandbox; the browser
bridge turns the local browser into the agent's browser. Both connect outbound
over WebSocket and are routed commands by the cloud agent tools.
"""
