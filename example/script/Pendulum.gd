extends Node2D

signal message_emitted(msg)

const SCALE := 64.0

@export var wasm_file := "" # (String, FILE, "*.wasm,*.wat")

@export var mass1 := 1.0 # (float, 0.001, 10)
@export var length1 := 1.0 # (float, 0.001, 10)
@export var mass2 := 1.0 # (float, 0.001, 10)
@export var length2 := 1.0 # (float, 0.001, 10)
@export var timestep := 0.001 # (float, 0.001, 1)

@export var angle1 := 0.0 : get = get_angle1, set = set_angle1 # (float, -180, 180, 0.01)
@export var velocity1 := 0.0 # (float, -10, 10, 0.01)
@export var angle2 := 0.0 : get = get_angle2, set = set_angle2 # (float, -180, 180, 0.01)
@export var velocity2 := 0.0 # (float, -10, 10, 0.01)

var instance: WasmInstance = null

@onready var shaft1 := $Shaft
@onready var bulb1 := $Bulb
@onready var pendulum2 := $Pendulum2
@onready var shaft2 := $Pendulum2/Shaft
@onready var bulb2 := $Pendulum2/Bulb

func get_angle1() -> float:
	return rad_to_deg(angle1)

func set_angle1(v: float) -> void:
	angle1 = deg_to_rad(v)

func get_angle2() -> float:
	return rad_to_deg(angle2)

func set_angle2(v: float) -> void:
	angle2 = deg_to_rad(v)

func _set_pendulum(
	shaft: Node2D,
	bulb: Node2D,
	length: float,
	weight: float,
	angle: float
) -> Vector2:
	var s := sin(angle)
	var c := cos(angle)
	var t := Transform2D(Vector2(c, -s), Vector2(s, c), Vector2.ZERO)

	shaft.transform = t * Transform2D(
		Vector2(min(weight, 1), 0),
		Vector2(0, length),
		Vector2.ZERO
	)

	bulb.transform = t * Transform2D(
		Vector2(weight, 0),
		Vector2(0, weight),
		Vector2(0, SCALE * length)
	)

	return t * Vector2(0, SCALE * length)

func _update_pendulum() -> void:
	var v := _set_pendulum(shaft1, bulb1, length1, mass1, angle1)
	pendulum2.position = v
	v = _set_pendulum(shaft2, bulb2, length2, mass2, angle2)

# Instance threadpool version
#func _ready():
#	var f: WasmFile = load(wasm_file)
#
#	var module = f.get_module()
#	if module == null:
#		__log("Cannot compile module " + wasm_file)
#		return
#
#	instance = InstanceHandle.new()
#	instance.instantiate(
#		module,
#		{},
#		{
#			"engine.use_epoch": true,
#			"engine.epoch_timeout": 1,
#		},
#		self, "__log"
#	)
#
#	instance.call_queue(
#		"setup",
#		[
#			mass1,
#			mass2,
#			length1,
#			length2,
#			timestep,
#			angle1,
#			velocity1,
#			angle2,
#			velocity2,
#		],
#		null, "",
#		self, "__log"
#	)
#
#var queued := 0
#
#func _process(delta):
#	if instance == null:
#		return
#
#	if queued < 3:
#		queued += 1
#		instance.call_queue(
#			"process", [delta],
#			self, "__update",
#			self, "__log"
#		)
#	else:
#		printerr("WASM Call takes too long! Maybe a bug?")
#
#func __log(msg: String) -> void:
#	emit_signal("message_emitted", msg)
#
#func __update(ret: Array) -> void:
#	angle1 = ret[0]
#	velocity1 = ret[1]
#	angle2 = ret[2]
#	velocity2 = ret[3]
#
#	_update_pendulum()
#
#	queued -= 1

# Non threadpool version
func _ready():
	var f: WasmFile = load(wasm_file)

	instance = f.instantiate()

	call_deferred("__setup")

func _process(delta):
	if instance == null:
		return

	var ret: Array = instance.call_wasm("process", [delta])
	angle1 = ret[0]
	velocity1 = ret[1]
	angle2 = ret[2]
	velocity2 = ret[3]

	_update_pendulum()

func __setup():
	if instance == null:
		return

	instance.call_wasm("setup", [
		mass1,
		mass2,
		length1,
		length2,
		timestep,
		angle1,
		velocity1,
		angle2,
		velocity2,
	])
