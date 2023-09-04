extends Control

signal message_emitted(msg)

@export_file("*.wasm", "*.wat") var wasm_file := ""

@onready var code_textbox: TextEdit = $Container/Panel/Margin/Tab/VBox/Code

@onready var file: WasmFile = load(wasm_file)
@onready var wasi_ctx: WasiContext = WasiContext.new()

func __check_file() -> bool:
	if file == null:
		printerr("Failed to import %s" % [wasm_file])
		return false
	return true

# Instance threadpool version
func _ready():
	if not __check_file():
		return

	var module := file.get_module()
	if module == null:
		__log("Cannot compile module " + wasm_file)
		return

#	wasi_ctx.connect("stdout_emit", self, "__log")
#	wasi_ctx.connect("stderr_emit", self, "__log")
	wasi_ctx.bypass_stdio = false
	wasi_ctx.write_memory_file(
		"hello_world.js",
		"console.log('Hello from Javascript!')",
		null
	)

func __log(msg: String) -> void:
	emit_signal("message_emitted", msg.strip_edges())

#func __run(source: String, ret_method = ""):
#	var module := file.get_module()
#	if module == null:
#		__log("Cannot compile module " + wasm_file)
#		return
#
#	var instance := InstanceHandle.new()
#	instance.instantiate(
#		module,
#		{},
#		{
#			"engine.use_epoch": true,
#			"engine.use_wasi": true,
#			"wasi.wasi_context": wasi_ctx,
#			"wasi.args": ["qjs.wasm", source],
#		}
#	)
#	instance.call_queue(
#		"_start",
#		[],
#		self if ret_method != "" else null,
#		ret_method
#	)

# Non threadpool version
func __run(source: String, ret_method = ""):
	if not __check_file():
		return

	var module := file.get_module()
	if module == null:
		__log("Cannot compile module " + wasm_file)
		return

	var instance = WasmInstance.new().initialize(
		module,
		{},
		{
			"engine.use_epoch": true,
			"engine.use_wasi": true,
			"wasi.wasi_context": wasi_ctx,
			"wasi.args": ["rustpython.wasm", source],
		}
	)
	var r = instance.call_wasm("_start", [])
	if ret_method != "":
		self.call(ret_method, r)

func __cb_write_file(_v):
	var b = wasi_ctx.read_memory_file("data/output.json", 1000, null)
	if not (b is PackedByteArray):
		__log("Cannot read file data/output.json")
		return
	var r := JSON.new()
	if r.parse(b.get_string_from_utf8()) != OK:
		__log("Error processing JSON")
		__log(r.get_error_message())
		return

	__log("data/output.json : %s" % r.data)

func __run_custom():
	if not __check_file():
		return

	wasi_ctx.write_memory_file("test.js", code_textbox.text, null)
	__run("test.js")
