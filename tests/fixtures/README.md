# Python JSON 浮点对照

`python_floats.tsv` 使用 CPython 3.14.4 的 `repr(float)` 生成，保存 IEEE-754 原始位模式和预期文本，Rust 测试不依赖 Python 运行时。

626 个值包括正负零、最大/最小正规值、最小次正规值、指数边界及相邻浮点、三个舍入中点案例，以及 `random.Random(20261019).getrandbits(64)` 生成的 512 个模式（排除非有限值）。指数样本为 -323、-308、-7、-6、-5、-4、-3、0、1、15、16、17、20、21、22、23、100、308。

格式依据为 [CPython 浮点 repr 实现](https://github.com/python/cpython/blob/3.14/Objects/floatobject.c)。回归同时检查默认/紧凑 separator 和 JSON 解析后的原始位模式；JSON 哈希样例另由 Python `json.dumps` + `hashlib.sha256` 计算。
