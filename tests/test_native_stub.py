"""The generated ``_native.pyi`` must track the built extension, name for name.

The completeness half of the stub drift gate: pyo3-stub-gen only sees
``gen_stub``-annotated items, so this suite pins the stub against the runtime —
every public module name, every class's property and method set, its base
class, its constructibility, and each callable's ``__text_signature__``
parameter names — catching any pyclass or pyfunction the annotation sweep
missed. The freshness half (committed == regenerated, byte for byte) is
``committed_stub_matches_generator_output`` in the ``stub_gen`` bin; regenerate
with ``cargo run -p cc-transcript-py --bin stub_gen``.
"""

from __future__ import annotations

import ast
import re
from pathlib import Path

from cc_transcript import _native

STUB = Path(_native.__file__).parent / "_native.pyi"


def stub_tree() -> ast.Module:
    return ast.parse(STUB.read_text())


def stub_classes(tree: ast.Module) -> dict[str, ast.ClassDef]:
    return {node.name: node for node in tree.body if isinstance(node, ast.ClassDef)}


def stub_functions(tree: ast.Module) -> dict[str, ast.FunctionDef]:
    return {node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)}


def class_members(node: ast.ClassDef) -> tuple[set[str], set[str], bool]:
    properties: set[str] = set()
    methods: set[str] = set()
    has_init = False
    for stmt in node.body:
        if not isinstance(stmt, ast.FunctionDef):
            continue
        if stmt.name == "__new__":
            has_init = True
        elif any(isinstance(d, ast.Name) and d.id == "property" for d in stmt.decorator_list):
            properties.add(stmt.name)
        elif not stmt.name.startswith("__"):
            methods.add(stmt.name)
    return properties, methods, has_init


def runtime_members(cls: type) -> tuple[set[str], set[str]]:
    getters = {k for k, v in vars(cls).items() if type(v).__name__ == "getset_descriptor"}
    methods = {
        k for k, v in vars(cls).items() if callable(v) and not k.startswith("__") and k not in getters
    }
    return getters, methods


def runtime_names() -> tuple[set[str], set[str]]:
    public = {n for n in dir(_native) if not n.startswith("_")}
    classes = {n for n in public if isinstance(getattr(_native, n), type)}
    return classes, public - classes


def test_every_runtime_name_is_stubbed_and_vice_versa() -> None:
    tree = stub_tree()
    classes, functions = runtime_names()
    assert classes == set(stub_classes(tree)), (
        f"runtime-only: {sorted(classes - set(stub_classes(tree)))}\n"
        f"stub-only: {sorted(set(stub_classes(tree)) - classes)}"
    )
    assert functions == set(stub_functions(tree)), (
        f"runtime-only: {sorted(functions - set(stub_functions(tree)))}\n"
        f"stub-only: {sorted(set(stub_functions(tree)) - functions)}"
    )


def test_class_properties_methods_and_bases_match_runtime() -> None:
    tree = stub_tree()
    for name, node in stub_classes(tree).items():
        cls = getattr(_native, name)
        getters, methods = runtime_members(cls)
        properties, stub_methods, has_init = class_members(node)
        assert properties == getters, f"{name} properties: stub {sorted(properties)} != runtime {sorted(getters)}"
        assert stub_methods == methods, f"{name} methods: stub {sorted(stub_methods)} != runtime {sorted(methods)}"
        runtime_base = cls.__mro__[1].__name__
        stub_bases = {base.id for base in node.bases if isinstance(base, ast.Name)}
        if runtime_base != "object":
            assert runtime_base in stub_bases, f"{name}: stub bases {stub_bases} miss runtime base {runtime_base}"
        assert has_init == (cls.__text_signature__ is not None), (
            f"{name}: stub {'declares' if has_init else 'omits'} __init__ but the runtime "
            f"{'has none' if cls.__text_signature__ is None else f'is constructible {cls.__text_signature__}'}"
        )


def signature_names(text_signature: str) -> list[str]:
    stripped = text_signature.strip("()")
    return [
        part.split("=")[0].split(":")[0].strip()
        for part in stripped.split(",")
        if part.strip() not in ("$self", "$cls", "/", "*", "")
    ]


def stub_param_names(args: ast.arguments) -> list[str]:
    names = [a.arg for a in (*args.posonlyargs, *args.args)]
    if args.vararg:
        names.append(f"*{args.vararg.arg}")
    names += [a.arg for a in args.kwonlyargs]
    if args.kwarg:
        names.append(f"**{args.kwarg.arg}")
    return names


def strip_receiver(params: list[str]) -> list[str]:
    """Drop only the receiver POSITION (first param when named self/cls), never later params —
    an extra caller-visible ``cls`` must stay so a wrong stub is caught."""
    return params[1:] if params and params[0] in ("self", "cls") else params


def test_strip_receiver_keeps_extra_cls_argument() -> None:
    fn = ast.parse("def f(self, cls, value): ...").body[0]
    assert isinstance(fn, ast.FunctionDef)
    stub_params = strip_receiver(stub_param_names(fn.args))
    assert stub_params == ["cls", "value"]
    assert stub_params != signature_names("($self, /, value)")


def test_optional_getters_declare_none() -> None:
    crate = Path(__file__).parent.parent / "rust" / "crates" / "py" / "src"
    liars = [
        f"{path.name}: {match.group(2)} declared {match.group(1)!r}"
        for path in crate.rglob("*.rs")
        for match in re.finditer(
            r'override_return_type\(type_repr = "((?:[^"\\]|\\.)*)"[^\]]*\)\s*\]\s*'
            r"(?:pub(?:\(crate\))? )?fn (\w+)[^{]*\{\s*opt_json\(",
            path.read_text(),
        )
        if not any(marker in match.group(1) for marker in ("None", "Any", "Optional"))
    ]
    assert not liars, f"opt_json getters whose stub type omits None: {liars}"


def test_callable_parameter_names_match_text_signatures() -> None:
    tree = stub_tree()
    for name, node in stub_functions(tree).items():
        sig = getattr(getattr(_native, name), "__text_signature__", None)
        if sig is None:
            continue
        stub_params = stub_param_names(node.args)
        assert stub_params == signature_names(sig), f"{name}: stub {stub_params} != runtime {sig}"
    for cls_name, node in stub_classes(tree).items():
        cls = getattr(_native, cls_name)
        for stmt in node.body:
            if not isinstance(stmt, ast.FunctionDef) or stmt.name.startswith("__"):
                continue
            if any(isinstance(d, ast.Name) and d.id == "property" for d in stmt.decorator_list):
                continue
            sig = getattr(vars(cls)[stmt.name], "__text_signature__", None)
            if sig is None:
                continue
            stub_params = strip_receiver(stub_param_names(stmt.args))
            assert stub_params == signature_names(sig), f"{cls_name}.{stmt.name}: stub {stub_params} != runtime {sig}"
