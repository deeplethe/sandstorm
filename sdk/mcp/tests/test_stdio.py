import sys

import anyio
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

EXPECTED_TOOLS = {
    "branch_sandbox",
    "delete_snapshot",
    "eval_code",
    "exec_command",
    "get_sandbox",
    "kill_sandbox",
    "list_sandboxes",
    "list_snapshots",
    "list_templates",
    "ping_sandbox",
    "spawn_sandboxes",
    "touch_sandbox",
    "wait_for_text",
}


def test_stdio_initialize_and_tool_registration() -> None:
    async def scenario() -> None:
        params = StdioServerParameters(
            command=sys.executable,
            args=["-m", "sandstorm_mcp.server"],
        )
        with anyio.fail_after(10):
            async with stdio_client(params) as (read_stream, write_stream):
                async with ClientSession(read_stream, write_stream) as session:
                    result = await session.initialize()
                    tools = await session.list_tools()

        assert result.serverInfo.name == "sandstorm"
        assert len(tools.tools) == len(EXPECTED_TOOLS)
        assert {tool.name for tool in tools.tools} == EXPECTED_TOOLS
        for tool in tools.tools:
            if tool.name in {"exec_command", "eval_code", "ping_sandbox", "wait_for_text"}:
                schema = tool.inputSchema
                token = schema["properties"]["sandbox_token"]
                assert token["default"] is None
                assert {item["type"] for item in token["anyOf"]} == {"string", "null"}
                assert "sandbox_token" not in schema.get("required", [])

    anyio.run(scenario)
