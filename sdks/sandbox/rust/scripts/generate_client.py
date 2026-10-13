# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0

"""Generate typed operation clients; transport policy remains in the shared runtime."""


def generate_client(g, snake, pascal, literal):
    operations = []
    for path, item in g.spec.get("paths", {}).items():
        item = g.resolve(item)
        for method, op in item.items():
            if method not in {"get", "put", "post", "delete", "patch", "head", "options", "trace"}:
                continue
            operation_id = op.get("operationId", snake(method + "_" + path))
            parameters = {}
            for parameter in item.get("parameters", []) + op.get("parameters", []):
                parameter = g.resolve(parameter)
                parameters[parameter["in"], parameter["name"]] = parameter
            operations.append((operation_id, path, method, op, list(parameters.values())))
    names = [snake(op[0]) for op in operations]
    if len(names) != len(set(names)) or len({pascal(op[0]) for op in operations}) != len(operations):
        raise ValueError("Colliding client operation names")
    by_id = {op[0]: op for op in operations}
    file_ids = {"getFilesInfo", "removeFiles", "chmodFiles", "renameFiles", "searchFiles",
                "replaceContent", "uploadFile", "downloadFile", "listDirectory", "makeDirs", "removeDirs"}
    scoped = any(op[0] in file_ids for op in operations)
    identity_present = any(op[0].startswith("identity") for op in operations)

    declarations, methods = [], []

    def path_expression(path, parameters, scope=None):
        values = {p["name"]: "params." + snake(p["name"]) for p in parameters if p["in"] == "path"}
        if scope == "identity":
            values.update(uid="uid", gid="gid")
        if scope == "isolated":
            values.update(sessionId="session_id")
        types = {p["name"]: g.resolve(p.get("schema", {})).get("type", "string") for p in parameters}
        if scope == "identity":
            types.update(uid="integer", gid="integer")
        if scope == "isolated":
            types.update(sessionId="string")
        segments = []
        for segment in path.strip("/").split("/"):
            if segment.startswith("{") and segment.endswith("}"):
                value = values[segment[1:-1]]
                numeric = types[segment[1:-1]] != "string"
                segments.append(f"(&{value}.to_string(), true)" if numeric else f"({value}, true)")
            else:
                segments.append(f"({literal(segment)}, false)")
        return "self.transport.operation_url(&[" + ", ".join(segments) + "])?"

    def contract(value):
        if isinstance(value, dict):
            return {key: contract(item) for key, item in value.items() if key not in {"description", "example", "examples", "summary", "title", "default", "format"}}
        if isinstance(value, list):
            return [contract(item) for item in value]
        return value

    for operation_id, path, method, op, parameters in operations:
        name = pascal(operation_id)
        fields_names = [snake(p["name"]) for p in parameters]
        if len(fields_names) != len(set(fields_names)):
            raise ValueError("Colliding client parameter names: " + operation_id)
        borrowed = any(g.resolve(p.get("schema", {})).get("type", "string") in {"string", "array"} for p in parameters)
        lifetime = "<'a>" if borrowed else ""
        fields = []
        for p in parameters:
            schema = g.resolve(p.get("schema", {}))
            typ = schema.get("type", "string")
            location = p["in"]
            if location not in {"path", "query", "header"}:
                raise ValueError("Unsupported client parameter location: " + location)
            if location == "path" and (not p.get("required") or typ == "array"):
                raise ValueError("Client path parameters must be required scalars")
            default_style = "form" if location == "query" else "simple"
            if p.get("style", default_style) != default_style:
                raise ValueError("Unsupported client parameter style")
            if location == "header" and typ != "string":
                raise ValueError("Client headers must be strings")
            if typ == "string":
                rust_type = "&'a str"
            elif typ == "array":
                if g.resolve(schema.get("items", {})).get("type") != "string":
                    raise ValueError("Unsupported client array parameter: " + p["name"])
                rust_type = "&'a [String]"
            elif typ == "integer":
                rust_type = "u64" if schema.get("minimum", -1) >= 0 else "i64"
            elif typ == "boolean":
                rust_type = "bool"
            else:
                raise ValueError("Unsupported client parameter: " + p["name"])
            if not p.get("required", False):
                rust_type = "Option<" + rust_type + ">"
            fields.append(f"    pub {snake(p['name'])}: {rust_type},")
        if fields:
            declarations.append("#[derive(Default)]\n#[allow(dead_code)]\npub(crate) struct " + name + "Params" + lifetime + " {\n" + "\n".join(fields) + "\n}")

        body = g.resolve(op.get("requestBody", {}))
        json_body = body.get("content", {}).get("application/json")
        args = ["&self"]
        if fields:
            args.append(f"params: {name}Params" + ("<'_>" if borrowed else ""))
        if json_body:
            body_type = g.type_for(json_body.get("schema", {}), name + "RequestBody" + (str(list(body["content"]).index("application/json") + 1) if len(body["content"]) > 1 else ""))
            if body_type.startswith("Vec<"):
                body_type = "[" + body_type[4:-1] + "]"
            args.append("body: " + ("&" + body_type if body.get("required") else "Option<&" + body_type + ">"))
        response_type = "()"
        for status, response in op.get("responses", {}).items():
            response = g.resolve(response)
            content = response.get("content", {}).get("application/json")
            if str(status).startswith("2") and content:
                response_type = g.type_for(content.get("schema", {}), name + "Response" + pascal(str(status)) + (str(list(response["content"]).index("application/json") + 1) if len(response["content"]) > 1 else ""))
                break
        lines = [f"    pub(crate) fn {snake(operation_id)}({', '.join(args)}) -> crate::Result<Call<{response_type}>> {{"]
        if operation_id in file_ids:
            if (identity_present and ("identity" + name) not in by_id) or ("isolated" + name) not in by_id:
                raise ValueError("Missing scoped filesystem operation: " + operation_id)
            identity_op = by_id.get("identity" + name)
            isolated_op = by_id["isolated" + name]
            for variant, scoped_names in [(identity_op, {"uid", "gid"}), (isolated_op, {"sessionId"})]:
                if variant is None:
                    continue
                common = [p for p in variant[4] if not (p["in"] == "path" and p["name"] in scoped_names)]
                if contract(common) != contract(parameters) or variant[2] != method or contract(g.resolve(variant[3].get("requestBody", {})).get("content", {}).get("application/json")) != contract(json_body):
                    raise ValueError("Scoped filesystem contract differs: " + variant[0])
            identity = identity_op[1] if identity_op else None
            isolated = isolated_op[1]
            lines += ["        let url = match self.scope {",
                      "            FilesystemScope::Default => " + path_expression(path, parameters) + ",",
                      "            " + ("FilesystemScope::Identity(uid, gid) => " + path_expression(identity, parameters, "identity") if identity else 'FilesystemScope::Identity(uid, gid) => { let _ = (uid, gid); return Err(crate::Error::InvalidConfig("Identity filesystem routes are unavailable in this branch".into())); }') + ",",
                      "            FilesystemScope::Isolated(session_id) => " + path_expression(isolated, parameters, "isolated") + ",",
                      "        };"]
        else:
            lines.append("        let url = " + path_expression(path, parameters) + ";")
        query = [p for p in parameters if p["in"] == "query"]
        if query:
            lines[1] = lines[1].replace("let url =", "let mut url =")
        for p in parameters:
            if p["in"] not in {"path", "query", "header"}:
                raise ValueError("Unsupported client parameter location: " + p["in"])
            schema = g.resolve(p.get("schema", {}))
            if p["in"] == "header" or (p["in"] == "path" and not any(k in schema for k in ["minimum", "maximum"])):
                continue
            field = "params." + snake(p["name"])
            optional = not p.get("required", False)
            lines.append(f"        {'if let Some(value) = ' + field if optional else 'let value = ' + field}{' {' if optional else ';'}")
            for key, comparison in [("minimum", "<"), ("maximum", ">")]:
                if key in schema and not (key == "minimum" and schema[key] == 0):
                    lines.append(f"            if value {comparison} {schema[key]} {{ return Err(crate::Error::InvalidConfig({literal(p['name'] + ' is outside its protocol range')}.into())); }}")
            if p["in"] == "query":
                if schema.get("type") == "array":
                    style = p.get("style", "form")
                    if style != "form":
                        raise ValueError("Unsupported client array style: " + style)
                    if p.get("explode", True):
                        lines.append(f"            for item in value {{ url.query_pairs_mut().append_pair({literal(p['name'])}, item); }}")
                    else:
                        lines.append(f"            url.query_pairs_mut().append_pair({literal(p['name'])}, &value.join(\",\"));")
                else:
                    value = "value" if schema.get("type") == "string" else "&value.to_string()"
                    lines.append(f"            url.query_pairs_mut().append_pair({literal(p['name'])}, {value});")
            if optional:
                lines.append("        }")
        lines.append(f"        let request = self.transport.request(reqwest::Method::{method.upper()}, url);")
        for p in parameters:
            if p["in"] == "header":
                field = "params." + snake(p["name"])
                lines.append(f"        let request = request.header({literal(p['name'])}, {field});" if p.get("required") else f"        let request = if let Some(value) = {field} {{ request.header({literal(p['name'])}, value) }} else {{ request }};")
        if json_body:
            lines.append("        let request = request.json(body);" if body.get("required") else "        let request = if let Some(body) = body { request.json(body) } else { request };")
        lines += ["        Ok(Call::new(request))", "    }"]
        methods.append("\n".join(lines))
    return "\n".join([
        "use super::" + "MODELS_MODULE" + "::*;",
        "use super::client::{Call, FilesystemScope};" if scoped else "use super::client::Call;",
        "use crate::transport::Transport;",
        "pub(crate) const SUPPORTS_IDENTITY_FILES: bool = " + str(identity_present).lower() + ";" if scoped else "",
        *declarations,
        "pub(crate) struct Client<'a> { transport: &'a Transport" + (", scope: FilesystemScope<'a>" if scoped else "") + " }",
        "#[allow(dead_code)]\nimpl<'a> Client<'a> {",
        "    pub(crate) fn new(transport: &'a Transport) -> Self { Self::filesystem(transport, FilesystemScope::Default) }" if scoped else "    pub(crate) fn new(transport: &'a Transport) -> Self { Self { transport } }",
        "    pub(crate) fn filesystem(transport: &'a Transport, scope: FilesystemScope<'a>) -> Self { Self { transport, scope } }" if scoped else "",
        *methods, "}",
    ])
