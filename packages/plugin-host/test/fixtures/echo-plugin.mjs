import { createInterface } from "node:readline";

const lines = createInterface({ input: process.stdin });
for await (const line of lines) {
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    respond(message.id, {
      contributions: {
        tools: [
          {
            id: "echo",
            description: "Echo structured input",
            input_schema: { type: "object" },
            output_schema: { type: "object" },
            effect_template: {},
          },
        ],
      },
    });
  } else if (message.method === "tool/invoke") {
    respond(message.id, { echoed: message.params.input });
  } else if (message.method === "shutdown") {
    respond(message.id, { stopped: true });
    process.exitCode = 0;
    lines.close();
  }
}

function respond(id, result) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
}
