# tinypipe — Execution Graph Platform (İzole Proje)

> **DO NOT IMPLEMENT** — Bu dosya henüz planlama aşamasındadır.
> Bu şuan yapılmayacak bu dosyayı sakın uygulama!!! Henüz planlaması ve mimari kararları bitmedi.

> **Mimari karar:** Execution graph platform, TinyOS'tan tamamen izole, ayrı bir workspace projesidir.
> Tıpkı `tinymachine` gibi (`/home/roy/github-projects/tinymachine/`), `tinypipe` de
> `/home/roy/github-projects/tinypipe/` altında bağımsız bir Rust workspace'i olarak geliştirilir.
> TinyOS, hem `tinypipe`'i hem de `tinymachine`'i ayrı ayrı path dependency ile tüketir.
> `tinypipe`, `tinymachine`'i **tanımaz** — tool dispatch `ToolRegistry` trait'i üzerinden
> TinyOS tarafından implemente edilir.
>
> ```
> tinymachine (sandbox/VM)           tinyos (agent/UI)
>    (izole)            ╲          ↗  (entegratör)
>                        ╲        /
>                      tinypipe (graph/compiler)
>                         (izole)
> ```
>
> **Hiçbir `tinypipe` crate'i `tinyos-*` veya `tinymachine-*` import etmez.** Sadece abstract trait'lere bağımlıdır.

> İş birimine tool'ları ver, LLM'le konuşarak ürün akışını oluştursun, onaylasın, production'a alsın.
> Geliştirici sadece tool'ları yazar. Ürün tanımı, karar mantığı, iş kuralları — tamamen iş biriminin kontrolünde.

---

## 1. Vizyon

İş birimi doğal dille anlatır, LLM execution graph'a çevirir, iş birimi onaylar, graph production'da deterministik çalışır.

```
İş birimi ──sohbet──▶ LLM ──Python Code──▶ rustpython_parser → Sanitize → Transform → Compiler ──▶ FlatBuffers IR ──▶ VM
                       ▲                        ▲                                        │
                       │                        │ (hata varsa satır söylenir,             │
                   (konuşarak düzelt)            │  LLM kendi kodunu düzeltir)             ▼
                                               Auto-Repair Loop                  Graph VM (deterministik)
                                                                                 LLM yok, hızlı, tutarlı
```

**Temel prensip:** LLM sadece tasarım aşamasında. Production'da LLM çağrılmaz. Her execution deterministik ve denetlenebilir.

---

## 2. Execution Graph

### 2.1 Tanım

Execution Graph, bir iş akışını tanımlayan yönlendirilmiş grafik (DAG)'tir. Her node bir işlemi, her edge bir veri akışını temsil eder.

### 2.2 Memory Model

Request boyunca yaşayan veri katmanları. **v1'de tek katman (flat context)**. **v2'de üç katman:**

| Katman | v1 | v2 | Açıklama |
|--------|----|----|----------|
| **Input** | — | var | Dışarıdan gelen, immutable. `customerId`, `plaka` hiç değişmez. |
| **Working Memory** | context (tek JSON) | var | Her node okur/yazar. `price`, `discount`, `hesapSonucu`. |
| **Output** | — | var | ACT node'larının ürettiği çıktı. Read-only (sadece ACT yazar). |

**v1:** Tüm veri tek JSON nesnesinde (`context`). Basit, hızlı.
**v2:** Immutable Input → Working Memory → Output. Debug'da "bu değer nereden geldi?" sorusu net cevaplanır. Debug ve audit için üç katman ayrı kaydedilir.

---

## 3. 11 Opcode

Bunlara "Atom" değil, **"Execution Opcode"** denir. Engine bunları yorumlayan küçük bir VM'dir.

### INPUT
Dışarıdan veri alır. Tip, isim, zorunluluk bilgisi taşır. Context'e yazar.

**Parametreler:** `name` (string), `type` (string), `required` (bool)

### CALL (önceden QUERY)
Bir tool'u, servisi, subgraph'ı, RPC'yi, fonksiyonu veya plugin'i parametrelerle çağırır. Parametreler context'ten referans alır (`$değer`, `$plaka`). Sonucu context'e yazar.

**Parametreler:** `target` (string — "tool:arac_sorgu", "subgraph:kasko_standart", "rpc:..."), `params` (map: key → "$context_ref"), `output_name` (string), `retry_count` (int, default: 0), `backoff_ms` (int, default: 100), `max_attempts` (int, default: 3), `timeout_ms` (int, default: 5000), `on_error` (enum: "abort" | "continue_with_null" | "continue_with_fallback", default: "abort"), `fallback_value` (optional JSON value, default: null)

**Partial Failure Stratejisi (`on_error`):** CALL node'u hata verdiğinde (tool timeout, 500, validation hatası), `on_error` parametresi nasıl davranılacağını belirler:

| `on_error` | Davranış | Kullanım Senaryosu |
|------------|----------|-------------------|
| `abort` (default) | Tüm execution'ı durdur, ERROR node'una git | Kritik işlemler (ödeme, onay) |
| `continue_with_null` | Hata durumunda `null` değerini context'e yaz, sonraki node'lara devam et | Opsiyonel veri (kampanya, puan) |
| `continue_with_fallback` | Hata durumunda `fallback_value`'yu context'e yaz, devam et | Varsayılan değer atanabilen işlemler (indirim oranı 0) |

```python
# on_error kullanım örnekleri:
def graph(plaka: str):
    # Kritik: abort — ödeme işlemi başarısız olursa tüm akış dursun
     odeme = call("odeme_yap", on_error="abort", ...)
    
    # Opsiyonel: continue_with_null — kampanya servisi çökerse null kullan
    kampanya = call("kampanya_sorgu", on_error="continue_with_null", ...)
    
    # Fallback: continue_with_fallback — indirim servisi hata verirse %0 kullan
    indirim = call("indirim_hesapla", 
                   on_error="continue_with_fallback", 
                   fallback_value=0, ...)
    
    # PARALLEL içinde partial failure: bir branch çökse diğerleri devam eder
    with parallel() as p:
        fiyat = p.run(call, "fiyatlandir", ...)
        puan = p.run(call, "puan_sorgu", on_error="continue_with_null", ...)
    # puan null ise de akış devam eder, fiyat kesintisiz hesaplanır
```

### CALC
Bir ifadeyi değerlendirir, sonucu context'e yazar. Context'teki değerleri referans alabilir.

**Parametreler:** `expr` (string: "2026 - $yıl"), `output_name` (string: "yaş")

### DECIDE
Bir context değerini operatörle karşılaştırır, true/false dalına gider.

**Parametreler:** `source` (string: "$değer"), `op` (enum: "eq", "neq", "gt", "gte", "lt", "lte", "contains"), `value` (any), `true_id` (string), `false_id` (string)

### SWITCH
Bir context değerini çoklu case'le karşılaştırır, eşleşen dala gider.

**Parametreler:** `source` (string: "$bölge"), `cases` (map: {"marmara": "node_kasko_standart", "ege": "node_kasko_ozel"}), `default_id` (string)

### ACT
Bir aksiyon/öneri üretir. Yan etki yaratabilir (kaydet, mail gönder, yönlendir).

**Parametreler:** `action_type` (string: "recommend", "save", "redirect", "notify"), `content` (string: "{{yaş}} yaş için {{paket}} öner"), `target` (opsiyonel string), `compensate` (opsiyonel string — ters işlem tool'u, v3)

### PARALLEL
Birden çok branch'i aynı anda çalıştırır.

**Memory isolation:** Her branch kendi **Local Scope Memory** kopyası ile başlar. Branch'ler aynı anda aynı değişkene yazarsa race condition olmaz — her branch kendi kopyasında yazar.

**⚠️ Static Validation kuralı: Branch'ler arası değişken erişimi yasaktır.** Bir branch içinde tanımlanan değişken, başka bir branch tarafından okunamaz. Sadece MERGE node'u branch'leri birleştirdikten sonra değişkenler üst scope'a geçer:

```python
# ✅ İZİN VERİLEN: Her branch kendi değişkenini tanımlar
def graph(x: int):
    with parallel() as p:
        branch1 = p.run(call, "tool_a", input=x)    # branch1 scope: {a}
        branch2 = p.run(call, "tool_b", input=x)    # branch2 scope: {b}
    # MERGE sonrası: üst scope'ta {a, b} var
    sonuc = a + b                                      # ✅ OK (MERGE sonrası)

# ❌ YASAK: Branch'ler arası çapraz erişim
def graph(x: int):
    with parallel() as p:
        a = p.run(call, "tool_a", input=x)
        b = p.run(call, "tool_b", input=a)             # ❌ COMPILER HATASI:
                                                        #    'a' Branch 2 scope'unda tanımlı değil
    sonuc = b

# Compiler: "HATA (5, 43): 'a' değişkenine Branch 2 içinden erişilemez.
#            'a' Branch 1'de tanımlanmıştır. Branch'ler bağımsız scope'lardır.
#            MERGE sonrası üst scope'ta kullanın."
```

**Kural özeti:**
- Her PARALLEL branch bağımsız bir `Scope` nesnesidir
- Branch içinde tanımlanan her değişken o branch'in scope'una aittir
- Bir branch başka bir branch'in değişkenine erişemez (compile-time hata)
- MERGE node'u tüm branch scope'larını birleştirir, üst scope'a açar
- MERGE öncesi hiçbir değişken üst scope'ta görünmez

**⚠️ Partial Failure ve Branch İzolasyonu:** PARALLEL içinde bir CALL `on_error="continue_with_null"` veya `continue_with_fallback` ile yapılandırılmışsa, o branch'in hatası diğer branch'leri etkilemez. Başarısız branch `null`/fallback değerini Local Scope'una yazar, MERGE'de diğer branch'lerin sonuçlarıyla birleştirilir:

```python
def graph(x: int):
    with parallel() as p:
        a = p.run(call, "tool_a", on_error="abort", ...)        # Hata = tüm PARALLEL durur
        b = p.run(call, "tool_b", on_error="continue_with_null", ...)  # Hata = b=null, devam
        c = p.run(call, "tool_c", ...)                          # Hata = tüm PARALLEL durur (default abort)
    
    # MERGE sonrası: a ve c başarılı olmalı (yoksa abort), b null olabilir
    sonuc = a + (b ?? 0)  # b null ise 0 kullan
```

**Static Validation kuralı (PARALLEL + on_error):**
```
PARALLEL içinde `on_error="continue_with_null"` olan CALL node'ları tespit edilir.
MERGE sonrası o değişkene erişimlerde null-check uyarısı verilir:
  "UYARI (7, 12): 'b' değişkeni PARALLEL içinde continue_with_null olarak
   işaretlenmiş. null olabilir. Kullanmadan önce kontrol edin: `b ?? default`"
```

**Parametreler:** `branches` (list of node_ids), `join_mode` (enum: "all", "any", "last"), `merge_strategy` (opsiyonel map, v2), `output_name` (string)

### LOOP
Bir body grubunu condition sağlanana kadar tekrarlar. Maksimum iterasyon sayısı tanımlanabilir.

**Termination analysis (compile-time):** Compiler LOOP gövdesini analiz eder. `until i >= 10` koşulu varsa ama gövdede `i` değişkeni mutasyona uğramıyorsa, compiler hata döndürür: *"LOOP break condition 'i >= 10' hiçbir zaman gerçekleşmeyecek — gövdede 'i' mutasyona uğramıyor."*

**Runtime budget:** FlatBuffers IR içinde her graph için bir `max_node_execution_count` (varsayılan: 10.000) bulunur. VM bu kotayı aşarsa execution'ı durdurur.

**⚠️ Memory growth uyarısı — LOOP içinde context birikmesi:** LOOP her iterasyonda context'e yeni değerler ekleyebilir (ör: `call("kayit_oku", index=i)` → `kayit_1, kayit_2, ...`). Bu durumda context boyutu iterasyon sayısıyla orantılı olarak büyür. `max_context_memory_bytes` (varsayılan: 10 MB) limiti, LOOP içinde de her CALL sonrası kontrol edilir — limit aşılırsa `ContextMemoryExceeded` hatası döner.

```python
# ❌ RİSKLİ: Her iterasyonda context büyür
def toplu_islem(dosya: str):
    satirlar = call("dosya_oku", path=dosya)
    for i in range(len(satirlar)):       # 1000 satır × 1KB/kayit = ~1MB context
        kayit = call("kayit_oku", index=i)
        islenen = call("islem_yap", veri=kayit)    # her CALL çıktısı context'te kalır
        act("LOG_KAYDET", mesaj=f"Islem {i+1}")
    # 1000 iterasyon sonunda context: 1000 + önceki değerler → max_context_memory_bytes aşılabilir

# ✅ GÜVENLİ: Aynı değişken her iterasyonda overwrite edilir
def toplu_islem(dosya: str):
    satirlar = call("dosya_oku", path=dosya)
    toplam_sonuc = 0
    for i in range(len(satirlar)):
        kayit = call("kayit_oku", index=i)
        islenen = call("islem_yap", veri=kayit)
        toplam_sonuc += islenen      # islenen her sefer overwrite edilir
    return {"toplam": toplam_sonuc}  # context: {satirlar, i, kayit, islenen, toplam_sonuc} — sabit boyut
```

**Compiler uyarısı (LOOP memory growth):** Compiler LOOP gövdesinde CALL node'larından output_name'lerin her iterasyonda overwrite edilip edilmediğini kontrol eder. Aynı output_name tekrar kullanılıyorsa uyarı yok; farklı output_name'ler birikiyorsa uyarı verir:

```
Compiler: "UYARI (4, 9): LOOP gövdesinde 'kayit' output ismi her iterasyonda
           değişiyor. Context boyutu iterasyon sayısıyla büyüyecek.
           Aynı değişkeni overwrite etmeniz önerilir."
```

**Parametreler:** `body_start` (string), `condition_node` (string — DECIDE döndürmeli), `max_iterations` (int, default: 100), `output_name` (string)

### WAIT
Belirli bir süre bekler veya belirli bir zamana kadar bekler.

**Parametreler:** `duration_secs` (int, max: 300 v1) veya `until` (string: ISO8601 timestamp)

### MERGE
PARALLEL'den gelen branch'lerin Local Scope Memory'lerini birleştirir.

**Birleştirme stratejisi (`mode`):**

| mode | Davranış | Kullanım |
|------|----------|----------|
| `all` | Tüm branch'lerin context'ini field-level union ile birleştir. Aynı field'a iki branch yazarsa: hata (v1) veya `merge_strategy` kullan (v2) | Tüm branch'lerin sonucu gerekiyorsa |
| `any` | İlk biten branch'in context'ini al, diğerlerini bekleme | En hızlı sonuç gerekiyorsa |
| `last` | Son biten branch'in context'ini al | Sıralı bağımlılık varsa |

**v2 field-level merge_strategy (opsiyonel):**
```json
{
  "mode": "all",
  "merge_strategy": {
    "fiyat": "min",
    "puan": "max",
    "hata_listesi": "concat",
    "musteri": "first"
  }
}
```

| Strateji | Davranış |
|----------|----------|
| `min` | Tüm branch'lerdeki en küçük değer alınır |
| `max` | En büyük değer alınır |
| `concat` | Tüm değerler array olarak birleştirilir |
| `first` | İlk yazan branch'in değeri alınır |
| `last` | Son yazan branch'in değeri alınır |

Belirtilmeyen field'lar için varsayılan strateji `last`'tir.

**v1'de PARALLEL + MERGE akışı:**

```
PARALLEL başlangıcı:
  Global Context kopylanır → Branch1: Local Scope (kopya)
                           → Branch2: Local Scope (kopya)
  Branch1 ve Branch2 eşzamanlı çalışır, her biri kendi Local Scope'una yazar

MERGE:
  mode = "any" → ilk biten branch'in Local Scope'u Global Context'e yazılır
  mode = "all" → tüm branch'ler beklenir, field-level union yapılır
```

### ERROR
Belirli bir node için hata yakalama tanımlar. Hata tipine göre farklı fallback node'a yönlendirebilir.

**Parametreler:** `on_node` (string), `fallback_node` (string), `error_types` (list of string: "timeout", "500", "validation", "all")

---

## 4. Graph Language + Compiler

LLM graph'ı doğrudan JSON AST olarak değil, **Restricted Python** kodu olarak üretir.
Python, LLM'lerin en yetenekli olduğu dildir (training data'da ~%20 pay, sıfırdan DSL'den 100× fazla örnek).
Özel bir DSL + Lexer + Parser yazmak yerine, olgunlaşmış `rustpython_parser` (Python'un resmi grammar'ını kullanan saf Rust parser) kullanıyoruz.

**Neden Restricted Python (sıfırdan DSL değil)?**

| Faktör | Sıfırdan DSL | Restricted Python |
|--------|-------------|-------------------|
| LLM syntax hatası | Sık (özel grammar'ı öğrenmesi gerekir) | **Çok nadir** (Python zaten "ana dili") |
| Parser geliştirme | Haftalarca grammar + lexer + parser yazımı | **Sıfır** (`rustpython_parser` hazır) |
| Parser hata payı | Yüksek (yeni yazılan parser'da bug olur) | **Düşük** (milyonlarca satır test edilmiş) |
| Determinizm | Kendi yazdığımız kadar | **%100** (Python grammar spec sabit) |
| Audit edilebilirlik | İş birimi öğrenmeli | **Okunabilir** (Python bilmeyen bile anlar) |
| Tooling | Sıfırdan hata mesajları, IDE desteği yok | Python ekosistemi (syntax highlighting, LSP) |

**Token verimliliği:** Python syntax'ı JSON'a göre ~%60 daha az token harcar (parantez ve tırnak yok), ama özel bir DSL'den sadece ~%10 daha fazla token harcar. Bu küçük fark, LLM'in sıfır syntax hatası yapması ve hazır parser kullanma avantajıyla fazlasıyla dengelenir.

```
LLM: Restricted Python Code         (LLM'in anadili, sıfır syntax hatası)
     │
     ▼
┌──────────────────────┐
│  rustpython_parser   │── Python grammar spec'e göre parse
│  (3.12 grammar)      │    çıktı: Python AST (Rust enum)
└──────┬───────────────┘
       │
       ▼
┌──────────────────────┐
│  AST Sanitization    │── Visitor Pattern ile Python AST'yi dolaş:
│  (Güvenlik Katmanı)  │    ✅ FunctionDef (sadece `graph` main)
│                      │    ✅ Call (sadece tool fonksiyonları)
│                      │    ✅ If, Compare, BinOp, Name, Constant
│                      │    ❌ Import, ClassDef, While, Lambda, Decorator
│                      │    ❌ eval(), exec(), __import__, os.*
│                      │    Geçemeyen düğüm → hata satırı + sebep
└──────┬───────────────┘
       │
       ▼
┌──────────────────────┐
│  Python AST →        │── Her Python AST düğümünü internal Opcode AST'ye
│  Opcode AST (IR)     │    dönüştür (örn: `call("tool", x=y)` → CALL node)
└──────┬───────────────┘
       │
       ▼
┌──────────┐
│  AST     │── Node list + Edge list + metadata (internal representation)
└────┬─────┘
     │
     ▼
┌──────────────┐
│  Static      │── cycle check, tool existence, type check
│  Validation  │    terminal check, input completeness
└──────┬───────┘
       │ (başarısız → hata satırı + sebep, LLM auto-repair)
       ▼
┌──────────────┐
│  Optimize    │
├──────────────┤
│ Constant     │── pi = 3.14 gibi sabit ifadeleri önceden hesapla
│ Folding      │
├──────────────┤
│ Dead Node    │── Hiçbir edge'de referans edilmeyen PURE node'ları kaldır
│ Elimination  │
├──────────────┤
│ Calc Fusion  │── Ardışık CALC'leri tek node'da birleştir:
│              │    a = x + 5; b = a * 2  →  b = (x + 5) * 2
├──────────────┤
│ Multi-Branch │── Aynı input'larla aynı CALL'ı yapan birden çok
│ Fusion       │    branch'i birleştir. Örn: paralel içinde iki kere
│              │    aynı tool aynı parametrelerle çağrılıyorsa → tek CALL
└──────┬───────┘
       │
       ▼
┌──────────────┐
│  Codegen     │── Opcode AST → FlatBuffers binary bytecode
│  (Backend)   │
└──────┬───────┘
       │
       ▼
┌──────────────────┐
│ FlatBuffers IR   │── Binary, zero-copy, O(1) random access
│ (execution.bfbs) │    Boyut: JSON'ın ~%20'si
└──────┬───────────┘
       │
       ▼
┌──────────┐
│ Graph VM │── sadece FlatBuffers IR okur, Python/JSON parse etmez
├──────────┤
│ Context  │
│ Stack    │
│ Sched.   │
│ Tool Run │
└──────────┘
```

**v1 (Python → Engine direkt interpreter):** Restricted Python → rustpython_parser → Sanitization → Opcode AST → Engine (direkt AST interpreter, FlatBuffers yok). Basit başlangıç.

**v2 (Python → Compiler → FlatBuffers → VM):** Restricted Python → rustpython_parser → Sanitization → Opcode AST → Validasyon → Optimize → Codegen → FlatBuffers → VM. Production'da zero-copy.

**Compiler iki alt sistemden oluşur:**
- **Frontend (Driver + Sanitizer + Validator):** LLM'den Python kodunu alır, `rustpython_parser` ile parse eder, AST Sanitization ile güvenlik kontrolü yapar, internal Opcode AST'ye dönüştürür, statik validasyondan geçirir. Hata varsa satır/sütun/sebep döndürür.
- **Backend (Optimizer + Codegen):** Valid Opcode AST'yi optimize eder, FlatBuffers binary'e derler.

#### Multi-Branch Fusion (Backend Optimization)

Compiler, aynı input'larla aynı CALL'ı yapan birden çok branch'i tespit eder ve birleştirir:

```python
# Restricted Python — iki branch aynı tool'u aynı parametreyle çağırıyor
def ornek(plaka: str):
    parallel {
        a = call("arac_sorgu", plaka=plaka)
        # ...
    } and {
        b = call("arac_sorgu", plaka=plaka)  # AYNI çağrı!
        # ...
    } merge all
}

# Compiler optimization → tek CALL'a indirgenir:
#   sonuc = call("arac_sorgu", plaka=plaka)  # 1 kere
#   parallel { a = sonuc ... } and { b = sonuc ... }
```

**Calc Fusion — ardışık CALC'leri birleştir:**

```python
# Restricted Python — ardışık atamalar
a = x + 5               # node_1: a = x + 5 (CALC)
b = a * 2               # node_2: b = a * 2 (CALC)
c = b + a               # node_3: c = b + a (CALC)

# Compiler optimization (CALC Fusion):
#   a = x + 5              → korunur (b ve c tarafından kullanılıyor)
#   b = (x + 5) * 2        → node_2 fused into node_1
#   c = ((x + 5) * 2) + (x + 5)  → node_3 fused into node_1 + node_2
```

**Fusion kuralları:**
- Sadece `pure = true` olan CALC ve CALL node'ları fusion'a katılabilir
- Aynı parametrelerle yapılan CALL'ların sonucu aynıdır (deterministik tool'lar)
- INPUT'a bağımlı node'lar asla fusion edilmez (her execution'da farklı input gelebilir)
- Fusion sonucu oluşan node sayısı kaydedilir: `optimizations: ["multi_branch_fusion", "calc_fusion"]`

### 4.1 Compiler Aşamaları

| Aşama | Girdi | Çıktı | Süre | Hata durumu |
|-------|-------|-------|------|-------------|
| **Parse (rustpython_parser)** | Python string | Python AST (Rust enum) | ~5-20µs | Python syntax hatası (LLM'de çok nadir) |
| **Sanitize (Visitor)** | Python AST | Sanitized Python AST | ~5-15µs | "Import kullanımı yasak (satır 3)", "eval() çağrısı engellendi" |
| **Transform** | Python AST | Opcode AST (internal) | ~3-10µs | "Beklenmeyen AST düğümü" |
| **Static Validation** | Opcode AST | Valid AST veya hata listesi | ~10-50µs | "Tool 'arac_sorgu' bulunamadı", "Tip uyuşmazlığı: string / int", "LOOP break değişkeni mutasyona uğramıyor" |
| **Optimization** | Valid AST | Optimize AST | ~10-100µs (opsiyonel) | Yok (sadece performans) |
| **Codegen** | Optimize AST | FlatBuffers binary | ~5-20µs | Yok (AST valid → codegen deterministic) |
| **Emit** | FlatBuffers binary | DB'ye blob olarak kaydet | ~1µs | Yok (I/O hatası hariç) |

**Hata → Auto-Repair döngüsü:**
```
Python syntax hatası (çok nadir, LLM Python'da neredeyse hiç hata yapmaz)
       │
       ▼
Sanitization hatası:
  Compiler: "HATA (3, 1): 'import' kullanımı yasak.
             Graph'ler tool çağrıları ve iş mantığı ile sınırlıdır."
       │
       ▼
LLM: "Özür dilerim, import kullanamıyormuşum." (kodu düzeltir)
       │
       ▼
Validation hatası:
Compiler: "Hata: 5. satırda 'mail_gonder' tool'u bulunamadı.
                 Kullanılabilir tool'lar: ['arac_sorgu', 'fiyatlandir']"
       │
       ▼
LLM: "Özür dilerim, yanlış tool adı kullandım. Düzeltiyorum:" (kodu yeniden üretir)
       │
       ▼
Compiler: (tekrar dene) → başarılı → AST

// Type error feedback örneği:
Compiler: "HATA (4, 12): Tip uyuşmazlığı
           'arac.marka' (string) / '2' (int) → bölme operatörü tanımsız"
       │
       ▼
LLM: "Hatalıyım, arac.marka bir string. arac.yil kullanmalıyım:"
       (kodu düzeltir)
       │
       ▼
Compiler: ✅ Tip kontrolü başarılı → AST
```

### 4.2 AST Sanitization (Güvenlik Katmanı)

`rustpython_parser` herhangi bir Python kodunu parse edebilir — `import os`, `eval()`, `__import__` dahil.
Güvenlik katmanı olarak **AST Sanitizer**, Python AST'sini Visitor Pattern ile dolaşır ve sadece izin verilen düğüm tiplerine geçit verir.

```rust
// tinypipe-compiler/src/sanitizer.rs — temsili kod
fn sanitize(node: &PythonAstNode) -> Result<(), SanitizationError> {
    match node {
        // ✅ İZİN VERİLEN DÜĞÜMLER
        PythonAstNode::FunctionDef { name, .. } if name == "graph" => {
            // Sadece "graph" adında bir fonksiyona izin ver
            Ok(())
        }
        PythonAstNode::Call { func, .. } => {
            // Sadece tool fonksiyonları ve bilinen yardımcılar
            let name = extract_call_name(func);
            if IS_ALLOWED_TOOL(name) || name == "parallel" || name == "act" {
                Ok(())
            } else {
                Err(SanitizationError::DisallowedCall(name))
            }
        }
        PythonAstNode::If { .. } => Ok(()),         // DECIDE için
        PythonAstNode::For { .. } => Ok(()),          // LOOP için
        PythonAstNode::Compare { .. } => Ok(()),      // if koşulları
        PythonAstNode::BinOp { .. } => Ok(()),        // matematik (CALC)
        PythonAstNode::UnaryOp { .. } => Ok(()),      // -x, not x
        PythonAstNode::Name { .. } => Ok(()),         // değişken referansı
        PythonAstNode::Constant { .. } => Ok(()),     // literal değerler
        PythonAstNode::Attribute { .. } => Ok(()),    // obj.field (nested context)
        PythonAstNode::Subscript { .. } => Ok(()),    // obj[key]
        PythonAstNode::Tuple { .. } | PythonAstNode::List { .. } => Ok(()),
        PythonAstNode::Pass { .. } => Ok(()),         // boş blok
        PythonAstNode::Return { .. } => Ok(()),       // function return
        
        // ❌ YASAKLI DÜĞÜMLER
        PythonAstNode::Import { .. } | PythonAstNode::ImportFrom { .. }
            => Err("Import kullanımı yasak!"),
        PythonAstNode::ClassDef { .. }
            => Err("Class tanımı yasak!"),
        PythonAstNode::While { .. }
            => Err("While döngüsü yasak! 'for' kullanın (max iteration var)."),
        PythonAstNode::Lambda { .. }
            => Err("Lambda ifadesi yasak!"),
        PythonAstNode::ListComp { .. } | PythonAstNode::DictComp { .. }
            => Err("Comprehension yasak!"),
        PythonAstNode::With { .. }
            => Err("With bloğu yasak!"),
        PythonAstNode::Try { .. }
            => Err("Try/except yasak! Graph'te 'try' statement kullanın."),
        PythonAstNode::Raise { .. }
            => Err("Exception fırlatmak yasak!"),
        PythonAstNode::AsyncFunctionDef { .. } | PythonAstNode::AsyncFor { .. }
            => Err("Async operasyonlar yasak!"),
        PythonAstNode::Yield { .. } | PythonAstNode::YieldFrom { .. }
            => Err("Generator yasak!"),
        PythonAstNode::Global { .. } | PythonAstNode::Nonlocal { .. }
            => Err("Scope manipülasyonu yasak!"),
        PythonAstNode::Delete { .. }
            => Err("Değişken silmek yasak!"),
        PythonAstNode::Match { .. }
            => Err("Python 3.10 match/case yasak! Graph'in 'switch' yapısını kullanın."),
        PythonAstNode::FString { .. }
            => Err("F-string yasak! 'act' içinde {{template}} kullanın."),
        PythonAstNode::Decorator { .. }
            => Err("Decorator yasak!"),
        PythonAstNode::WalrusOperator { .. }
            => Err("Walrus operator ':=' yasak!"),

        _ => Err(format!("Desteklenmeyen Python düğümü: {:?}", node)),
    }
}

/// Graph'in ana fonksiyonu dışında yardımcı fonksiyonlar da olabilir
fn validate_function(name: &str) -> Result<(), SanitizationError> {
    if name == "graph" { Ok(()) }
    else { Err(SanitizationError::DisallowedFunction(name)) }
}
```

**Sanitizasyon kuralları (özet):**

| Kategori | İzin Verilen | Yasaklanan |
|----------|-------------|------------|
| **Kontrol akışı** | `if/elif/else`, `for` (sabit range), `return` | `while`, `try/except`, `match/case`, `async for` |
| **İfadeler** | `call()`, `act()`, matematik, karşılaştırma, `and`/`or`/`not` | `eval()`, `exec()`, `lambda`, comprehension, walrus |
| **Değişkenler** | Atama, attribute erişim, subscript | `global`, `nonlocal`, `del` |
| **Modüller** | Sadece built-in tool fonksiyonları | `import`, `from`, `__import__`, `importlib` |
| **Tanımlar** | Sadece `def graph(...)` | `class`, `async def`, decorator |
| **Literaller** | String, int, float, bool, None, list, tuple, dict | F-string (template syntax kullan) |
| **Yan etkiler** | `act()` (kontrollü) | `print()`, `open()`, `input()`, `os.*`, `sys.*` |

**Sanitizer hata mesajı örneği:**

```
━━━ Compiler Feedback ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  GÜVENLİK HATASI (6, 5): Yasak Python düğümü
  'import' kullanımı yasak! Graph'ler tool çağrıları ve iş
  mantığı ile sınırlıdır. Tüm bağımlılıklar Tool Registry
  üzerinden sağlanır.
  
  Kod:
    6   │  import json
        │  ^~~~~~ burada hata
  
  Öneri: json işlemleri için tool yazdırın veya doğrudan
  Python dict/list operasyonları kullanın (izinli).
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

### 4.3 Opcode AST (Internal Representation)

Sanitized Python AST, Transform aşamasında internal **Opcode AST**'ye dönüştürülür.
Bu, 11 opcode'lu FlatBuffers IR'ye köprü görevi görür. Validasyon ve optimizasyon bu yapı üzerinde yapılır.

**Python AST → Opcode AST dönüşüm kuralları:**

| Python Construct | → | Opcode AST Node |
|-----------------|---|-----------------|
| `def graph(inputs):` | → | INPUT nodes (her input parametresi için) |
| `result = call("tool", ...)` | → | CALL node |
| `result = expr` (matematik) | → | CALC node |
| `if cond: ... else: ...` | → | DECIDE node + edges |
| `act("TYPE", ...)` | → | ACT node |
| `for x in range(N):` | → | LOOP node + body |
| `parallel([f1(), f2()])` | → | PARALLEL node + branches |
| `return result` | → | Edge to OUTPUT |

#### 4.3.1 Control Flow Flattening (CFG → DAG)

Python'da `if` içinde `return` (erken çıkış), iç içe `if/else`, zincirleme koşullar gibi **non-linear control flow** desenleri vardır. Bunları Opcode AST'nin DAG yapısına indirgemek için bir **Control Flow Graph (CFG) Flattening** aşaması gerekir.

**Problem:** Doğrudan dönüşümde erken `return` DAG'i bozar:

```python
def graph(plaka: str):
    arac = call("arac_sorgu", plaka=plaka)
    if arac.hasarli:
        act("NOTIFY", msg="Hasarlı araç reddedildi")
        return              # ← Erken çıkış: alt akışa gitme!
    
    fiyat = call("fiyatlandir", arac=arac)    # ← Buraya sadece hasarli=False ise gel
    act("RECOMMEND", fiyat=fiyat)
```

**Çözüm — CFG Flattening (Transform aşamasında):**

```
Python AST
    │
    ▼
┌──────────────────────────┐
│  Control Flow Graph      │── Her Python bloğu (if/else/for/return)
│  (CFG)                   │    basic block'lara ayrılır. Her block'un
│                          │    bir entry point'i ve exit edge'leri vardır.
│                          │    return = block'un sonu, sonraki block'a edge yok.
└────────┬─────────────────┘
         │
         ▼
┌──────────────────────────┐
│  CFG → DAG Flattening    │── Her basic block'u bir Opcode AST node'una
│                          │    dönüştür. Erken return'leri MERGE node ile
│                          │    handle et: return eden yol OUTPUT'a gider,
│                          │    return etmeyen yol sonraki block'a.
└────────┬─────────────────┘
         │
         ▼
┌──────────────────────────┐
│  Opcode AST (DAG)        │── Artık tüm node'lar DAG'e uygun: return
│                          │    edge'lerde condition ile ifade edilir
└──────────────────────────┘
```

**Yukarıdaki örneğin flattened Opcode AST karşılığı:**

```
INPUT(plaka)
    │
    ▼
CALL(arac_sorgu) → output: arac
    │
    ▼
DECIDE(kosul: arac.hasarli)
    ├── TRUE  → ACT(notify) → OUTPUT (erken return)
    │                   │
    └── FALSE ──────────┘
                │
                ▼
        CALL(fiyatlandir) → output: fiyat
                │
                ▼
        ACT(recommend) → OUTPUT
```

**CFG Flattening kuralları:**

| Python Deseni | CFG'de | DAG'de |
|--------------|--------|--------|
| `if cond: A; return; B` | Block(cond→A→exit), Block(B) | DECIDE(cond, TRUE→A→OUTPUT, FALSE→B→OUTPUT) |
| `if cond: A else: B; C` | Block(cond→A→C), Block(cond→B→C) | DECIDE(cond, TRUE→A→MERGE, FALSE→B→MERGE), MERGE→C |
| `for x in R: body` | Loop(header→body→header→exit) | LOOP(body, condition) |
| `if c1: A; if c2: B` (nested) | Block(c1→A→c2→B), Block(c1→¬c2) | DECIDE(c1)→DECIDE(c2) chain |

**Flattening sonrası DAG'de şu invariant sağlanır:**
- Hiçbir node'un birden çok çıkışı yoktur (DECIDE hariç: 2 çıkış)
- Hiçbir node birden çok kez ziyaret edilmez (DAG)
- Erken `return` bir OUTPUT edge'i ile temsil edilir, DECIDE'ın TRUE/FALSE dallarından birine bağlanır

**v1'de CFG Flattening basit tutulur:** Sadece tek seviye `if/else` + `return`. İç içe `if` (nested) v2'ye bırakılır. v2'de tam CFG analizi gelir.

```rust
// tinypipe-compiler/src/ast.rs (Rust representation)
struct AstGraph {
    version: u16,
    nodes: Vec<AstNode>,
    edges: Vec<AstEdge>,
    metadata: AstMetadata,
}

struct AstNode {
    id: String,
    op: Opcode,        // enum: INPUT, CALL, CALC, DECIDE, ...
    pure: bool,        // true = safe to eliminate if unreferenced
    inferred_type: Option<Type>,  // Tool Registry output_schema'dan çıkarılan tip (varsa)
    args: HashMap<String, ArgValue>,  // henüz type-check edilmemiş
}

/// Tip sistemi — JSON Schema tipleri + composite types
enum Type {
    String,
    Int,
    Float,
    Bool,
    Object(HashMap<String, Type>),  // nested object
    Array(Box<Type>),               // homojen array
    Any,                            // henüz bilinmiyor / dinamik
}

struct AstEdge {
    from: String,
    to: String,
    condition: Option<String>,   // None = unconditional
}

enum Opcode {
    Input, Call, Calc, Decide, Switch,
    Act, Parallel, Loop, Wait, Merge, Error,
}
```

**Purity (Side-Effect) Inference — `pure` flag'ı nasıl belirlenir:**

| Opcode | pure | Açıklama |
|--------|------|----------|
| INPUT | `true` | Sadece context'e yazar, dış dünyaya etkisi yok |
| CALC | `true` | Saf matematik, yan etki yok |
| DECIDE | `true` | Sadece context'ten okur, dal seçer |
| SWITCH | `true` | DECIDE ile aynı |
| WAIT | `true` | Zaman bekler, yan etki yok |
| MERGE | `true` | Sadece context birleştirir |
| CALL | tool'a bağlı | Tool Registry'deki `pure` flag'ına bakar (varsayılan: `false`) |
| ACT | `false` | Her zaman impure (mail, kaydet, yönlendirme — dış etki) |
| PARALLEL | içerdiği node'lara bağlı | Tüm child'lar pure ise pure |
| LOOP | içerdiği node'lara bağlı | Body'deki tüm node'lar pure ise pure |
| ERROR | `false` | Hata durumunda dış etki olabilir |

`pure = false` olan bir node, **hiçbir edge tarafından referans edilmese bile silinemez.** Çünkü dış dünyaya etkisi vardır (örneğin: mail gönderen ACT node'unun output'u hiçbir yerde kullanılmasa da mail gitti).

**Dead Node Elimination (güncellenmiş kural):**
> "Hiçbir edge'de referans edilmeyen **VE pure = true olan** node'ları kaldır."

Bu sayede:
- `CALC yas_hesapla` (pure) → hiçbir edge referans etmiyorsa silinir ✅
- `ACT mail_gonder` (impure) → hiçbir edge referans etmese de korunur ✅
- `CALL fiyat_hesapla` (impure) → Tool Registry'de `pure: false` ise korunur ✅

**Subgraph Cyclic Dependency Detection — Compile-Time:** Compiler, tüm subgraph'ların CALL graph'ını **Global Call Graph DFS** ile analiz eder. Subgraph'lar arasında döngüsel bağımlılık (A→B→C→A) varsa compile-time hata döndürülür. Bu, subgraph'lar farklı ekipler tarafından yazılsa bile production'da sonsuz recursion'ı önler.

```python
# ❌ YASAK: Cyclic subgraph dependency
# graph_a.py:
def graph(x: int):
    result = call("subgraph:graph_b", x=x)  # → graph_b'yi çağırır
    return result

# graph_b.py:
def graph(x: int):
    result = call("subgraph:graph_a", x=x)  # → graph_a'ya geri döner!
    return result  # ← COMPILER HATASI: "Cyclic subgraph dependency: graph_a → graph_b → graph_a"
```

**Global Call Graph algoritması:**
```
1. Tüm subgraph'ların CALL target'larını tara (tool: / subgraph:)
2. Subgraph'tan subgraph'a edge'lerden oluşan bir yönlü grafik oluştur
3. DFS ile her subgraph'tan başlayarak döngü ara
4. Döngü tespit edilirse: hangi path'te cycle olduğunu gösteren hata mesajı
5. `max_subgraph_depth` (Metadata) aşılırsa uyarı: "Subgraph nesting depth (X) max (Y)'ı aştı"
```

**FlatBuffers Metadata'da `max_subgraph_depth`:** Compile-time'da subgraph iç içe geçme derinliğini sınırlamak için yeni alan (runtime `max_recursion_depth`'ten farklıdır — compile-time uyarı, runtime hard limit):

```fbs
table Metadata {
    // ... mevcut alanlar ...
    max_recursion_depth: uint = 5;       // runtime: subgraph çağrı derinliği hard limit
    // ...
}
```

`max_recursion_depth` hem compile-time uyarı (subgraph nesting > 5 ise "Çok derin subgraph nesting, önerilen max: 5") hem runtime hard limit olarak çalışır.

**Subscript Restrictions — Dinamik İndisleme Yasağı:**

Python'da dict/list subscript'leri dinamik olabilir. Static type güvenliğini korumak için:

| Subscript Deseni | Örnek | İzin | Gerekçe |
|-----------------|-------|------|---------|
| **String literal key** | `arac["marka"]` | ✅ İzinli | Compiler tipini `output_schema`'dan çıkarabilir |
| **Int literal index** | `liste[0]` | ✅ İzinli | Sabit indis, tip biliniyor |
| **Constant variable** | `KEY = "marka"; arac[KEY]` | ✅ İzinli | Compiler constant folding ile çözer |
| **Dinamik key** | `arac[dinamik_key]` | ❌ **Yasak** | Compiler tipi bilemez, runtime TypeError riski |
| **Değişken index** | `items[i]` (`i` loop variable) | ⚠️ **Uyarı** | `items` Array tipindeyse izinli, Object ise yasak |

```python
# ✅ İZİN VERİLEN subscript'ler
def graph(plaka: str):
    arac = call("arac_sorgu", plaka=plaka)   # → {"marka": str, "yil": int}
    
    # Attribute erişimi (output_schema'da tanımlı)
    model = arac.marka                          # ✅ → str
    
    # String literal key (output_schema'dan tip çıkarılır)
    marka = arac["marka"]                       # ✅ → str
    yil = arac["yil"]                           # ✅ → int
    
    # Int literal index (Array tipinde)
    liste = [1, 2, 3]
    ilk = liste[0]                              # ✅ → int

# ❌ YASAK: Dinamik key
def graph(plaka: str):
    arac = call("arac_sorgu", plaka=plaka)
    key = "marka" if plaka == "34ABC123" else "model"
    deger = arac[key]                            # ❌ COMPILER HATASI
                                                  # "Dinamik key ile subscript yasak.
                                                  #  output_schema'daki alan adlarını
                                                  #  doğrudan kullanın: arac.marka"
```

**Tool Registry'den Tip Çıkarımı:**

```rust
// Type inference kuralları
fn infer_type(node: &AstNode, registry: &ToolRegistry) -> Type {
    match node.op {
        Opcode::Input => {
            // INPUT tipi graph tanımından gelir: "yas: int" → Type::Int
            registry.get_input_type(node.id)
        }
        Opcode::Call => {
            // CALL tipi Tool Registry'deki output_schema'dan gelir
            let tool_name = node.args.get("target").unwrap();
            registry.get_output_type(tool_name)
        }
        Opcode::Calc => {
            // CALC tipi operand tiplerinden çıkarılır
            // Toplama: int + int → int, float + float → float
            // Bölme: int / int → float (her zaman)
            // String + int → TYPE_ERROR
            infer_calc_type(node.args.get("expr"))
        }
        Opcode::Decide | Opcode::Switch => {
            // DECIDE/SWITCH her zaman bool döndürür (dal seçer)
            Type::Bool
        }
        Opcode::Act => {
            // ACT çıktı üretmez, sadece yan etki yaratır
            Type::Any
        }
        // ... diğer opcode'lar
    }
}
```

**Type Error örnekleri — compile-time yakalanır:**

```python
# ❌ String / Int bölmesi
islem = arac.marka / 2
# Compiler: "HATA (4, 12): 'arac.marka' (string) ve '2' (int) arasında
#            bölme işlemi yapılamaz."

# ❌ Call'a yanlış tip parametre
sonuc = call("fiyatlandir", yas="otuzbes")
# Compiler: "HATA (5, 30): 'fiyatlandir.yas' parametresi int bekliyor,
#            string verildi."

# ❌ DECIDE'da string karşılaştırma
if arac.deger > "yuksek":
    ...
# Compiler: "HATA (6, 18): '>' operatörü int ve string arasında
#            kullanılamaz."

# ✅ Tip güvenli kod
yas = call("kullanici_sorgu", id=input.user_id).yas  # → int
if yas < 25:                                           # ✅ int < int
    ...
```

**Tip çıkarımı sayesinde LLM şu feedback'i alır:**
```
━━━ Compiler Feedback ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  HATA (4, 12): Tip uyuşmazlığı
  'arac.marka' (string) / '2' (int) → bölme operatörü tanımsız
  
  Kod:
    4  │  islem = arac.marka / 2
       │                  ^~~~~~~~~~~
  
  Öneri: 'arac.marka' bir string. Sayısal işlem için
  'arac.yil' (int) veya 'arac.deger' (float) kullanın.
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

**AST'te validasyon şu hataları yakalar:**
- `DECIDE.true_id` = "node_x" ama graph'ta "node_x" diye node yok
- `CALL.target` = "tool:olmayan_tool" ama Tool Registry'de kaydı yok
- Edge'de cycle var (DFS ile tespit)
- Bir dal ACT ile bitmiyor
- **Tip uyuşmazlığı:** CALC'de string * int, CALL'a yanlış tip parametre
- **LOOP break condition:** `until i >= 10` ama gövdede `i` mutasyona uğramıyor (sonsuz döngü riski)

### 4.4 FlatBuffers Bytecode / IR

Codegen aşaması AST'yi alır ve **FlatBuffers binary**'e dönüştürür. FlatBuffers, Google tarafından geliştirilmiş bir zero-copy serialization formatıdır.

**Avantajları:**
- **Zero-copy deserialization:** Binary'den okuma yaparken allocation olmaz. VM buffer'a direkt pointer alır.
- **O(1) random access:** Node'a ID'si ile erişim hash map gerektirmez, doğrudan offset tablosundan okunur.
- **Memory-mapped:** DB'den okunan binary direkt mmap ile VM'e verilebilir.
- **Boyut:** JSON'a göre ~%80 daha küçük (string'ler bir kere yazılır, etiketler int enum).

```fbs
// execution_plan.fbs — FlatBuffers schema for compiled graph

table ExecutionPlan {
    version: ushort = 2;
    nodes: [Node];
    edges: [Edge];
    metadata: Metadata;
}

table Node {
    id: string;            // "giris", "fiyat_hesapla"
    op: Opcode;            // enum olarak, 1 byte
    args: [Arg];           // key-value çiftleri
}

enum Opcode : ubyte {
    Input = 0,
    Call = 1,
    Calc = 2,
    Decide = 3,
    Switch = 4,
    Act = 5,
    Parallel = 6,
    Loop = 7,
    Wait = 8,
    Merge = 9,
    Error = 10,
}

table Arg {
    key: string;
    value: string;    // JSON-encoded değer (string, number, array, object)
}

table Edge {
    from: string;
    to: string;
    condition: string;  // boş string = unconditional
}

table Metadata {
    compiler_version: string;
    compiled_at: string;       // ISO8601
    node_count: uint;
    edge_count: uint;
    max_node_execution_count: uint = 10000;  // VM execution budget (sonsuz döngü koruması)
    max_context_memory_bytes: uint = 10485760;  // 10 MB — context/heap memory limit (OOM koruması)
    max_recursion_depth: uint = 5;               // Maks subgraph çağrı derinliği (sonsuz recursion koruması)
    max_execution_time_ms: uint = 30000;         // **YENİ** Wall-clock timeout: 30sn (instruction cycle budget)
    optimizations: [string];   // ["constant_folding", "dead_node_elimination", "multi_branch_fusion"]
    source_graph_id: string;   // hangi graph'tan derive edildi
    source_graph_version: uint;
    tool_dependencies: [ToolDep];  // tool bağımlılıkları (semver pin)
}

table ToolDep {
    name: string;        // "arac_sorgu"
    version: string;     // "^1.0.0" semver constraint
    pure: bool;
    schema_hash: string = "";  // SHA256 of (input_schema + output_schema) — compile anındaki şema hash'i
}

root_type ExecutionPlan;
```

#### 4.4.1 Tool Schema Versioning & Validation

**Problem:** Tool Registry'deki bir tool'un `input_schema` veya `output_schema`'sı, graph compile edildikten sonra değişirse ne olur?

```
Gün 1: Graph compile edilir → tool "arac_sorgu" output_schema = {"marka": string, "yil": int}
Gün 2: Tool güncellenir → output_schema = {"marka": string, "model": string, "yil": int, "hasar_gecmisi": object}
Gün 30: Graph VM'de çalışır → CALL("arac_sorgu") → yeni field "hasar_gecmisi" gelir
         → CALC sonrası context'te beklenmeyen alan → runtime hatası veya sessiz hata
```

**Çözüm — `tool_version_hash`:** Her graph'ın FlatBuffers Metadata'sı, compile anındaki tool şemalarının hash'ini içerir. VM, CALL dispatch'ten önce bu hash'i Tool Registry'deki güncel şema ile karşılaştırır:

```fbs
table ToolDep {
    name: string;             // "arac_sorgu"
    version: string;          // "^1.0.0" (semver constraint)
    pure: bool;
    schema_hash: string;      // SHA256 of (input_schema + output_schema) — compile anındaki şema
}
```

**VM Runtime Validation (CALL dispatch öncesi):**

```
VM: CALL("tool:arac_sorgu") dispatch etmeden önce:
  1. ToolDep.schema_hash (compile anındaki şema hash'i) oku
  2. Registry'deki tool'un güncel (input_schema + output_schema) hash'ini hesapla
  3. Karşılaştır:
     → EŞİT: "Şema değişmemiş → dispatch et" ✅
     → FARKLI: "⚠️ Şema değişmiş! Graph yeniden compile edilmeli."
        └─ Schema uyumluluğunu kontrol et:
           ├─ Yalnızca yeni optional field eklenmiş → devam et (genişleme uyumlu) 
           └─ Required field eklenmiş / field silinmiş → VM hata döndürür:
              "Tool 'arac_sorgu' şeması değişti. Graph yeniden compile edilmeli."
```

**Schema drift tespiti genişleme uyumu (backward-compatible schema changes):**

| Değişiklik | VM Davranışı |
|------------|-------------|
| Yeni optional field eklendi | ✅ Devam et (tool yeni field döndürür, graph eski field'ları okur) |
| Yeni required field eklendi | ❌ Hata: "Şema değişikliği — graph compile'ı gerekli" |
| Field silindi | ❌ Hata (graph'ın kullandığı field artık yok) |
| Field tipi değişti | ❌ Hata (ör: int → string) |
| Field opsiyonel → required | ❌ Hata (graph eski schema'ya göre compile edilmiş) |
| Enum'a yeni değer eklendi | ⚠️ Uyarı (tool yeni enum döndürebilir, graph'ın DECIDE'ı kapsamıyor olabilir) |

**Auto-Repair tetikleyici:** VM şema değişikliği tespit ettiğinde:
1. Execution'ı `failed` olarak işaretle, sebep: "tool_version_mismatch"
2. Orchestrator otomatik olarak LLM'e mesaj gönder:
   "`arac_sorgu` tool'unun şeması değişti. Graph'ı günceller misin?"
3. LLM yeni şemaya göre kodu günceller, yeni graph versiyonu compile edilir
4. Yeni versiyon deploy edilir, execution tekrar başlatılır

#### 4.4.2 Version Compatibility

FlatBuffers IR, zaman içinde yeni opcode'lar veya alanlar eklendiğinde geriye dönük uyumluluğu korumalıdır. **Temel prensip:** Eski bir VM yeni bir IR'i çalıştıramayabilir (bilmediği opcode), ama yeni bir VM **her zaman eski IR'leri çalıştırabilmelidir** (geriye dönük uyumluluk).

##### Schema Field ID Kararlılığı

FlatBuffers'ta her table field'ı bir `@N` ID'si taşır (sıralı, 0'dan başlar). Bu ID'ler **asla değiştirilmez** ve **asla yeniden kullanılmaz**:

```fbs
// v1 schema (field ID'ler @0, @1, @2)
table Node {
    id: string;          // @0 — asla başka bir field'a verilmez
    op: Opcode;          // @1 — asla başka bir field'a verilmez
    args: [Arg];         // @2 — asla başka bir field'a verilmez
}

// v2 schema (YENİ field SADECE sona eklenir: @3, @4)
table Node {
    id: string;          // @0 — değişmez
    op: Opcode;          // @1 — değişmez
    args: [Arg];         // @2 — değişmez
    pure: bool = true;   // @3 — YENİ: varsayılan true (eski IR'ler bu field'ı içermez → default)
    cost_estimate_us: uint = 0;  // @4 — YENİ: varsayılan 0
}

// ❌ YASAK: Field ID'yi yeniden kullanmak veya sırayı değiştirmek
// table Node {
//     op: Opcode;          // @0 ❌ 'id' iken 'op' oldu — tüm eski IR'ler bozulur!
//     id: string;          // @1 ❌
//     args: [Arg];         // @2
//     pure: bool = true;   // @3
// }
```

**Kural seti:**

| Kural | Açıklama | İhlal edilirse |
|-------|----------|----------------|
| **Enum'a yeni değer SADECE sona eklenir** | `Opcode` veya başka bir enum'a yeni variant eklenirken explicit value verilir: `Suspend = 12` | Eski VM yeni enum'u `unknown` olarak okur, hata fırlatır |
| **Enum değerleri asla değiştirilmez** | `Act = 5` → ileride `Act = 42` yapılmaz | Tüm eski IR'ler bozulur |
| **Field ID'ler (`@N`) asla değiştirilmez, asla yeniden kullanılmaz** | Bir field silinirse ID'si `deprecated` olarak işaretlenir, başka bir field'a verilmez | Binary layout bozulur, tüm eski IR'ler okunamaz |
| **Yeni field'lar SADECE sona eklenir** | Mevcut en yüksek `@N`'den sonraki ID ile eklenir | Eski VM yeni field'ı görmez, default değer kullanır |
| **Yeni field'lar `optional` veya default değerli olmalıdır** | `table Node { ...; pure: bool = true; }` — eski IR'lerde bu field yok → varsayılan kullanılır | Eski IR'ler çözülemez |
| **Table silinmez, `deprecated` yapılır** | Eski bir table tipi kaldırılacaksa `// @deprecated` yorumu eklenir | Eski IR'ler çözülemez |

##### ExecutionPlan.version ve VM Yükleme Protokolü

Her FlatBuffers IR, `ExecutionPlan.version` alanında compile edildiği compiler versiyonunu taşır. VM, IR'i yüklerken versiyonu kontrol eder:

```fbs
table ExecutionPlan {
    version: ushort = 2;   // Compiler versiyonu. VM buna göre dispatch stratejisi belirler.
    nodes: [Node];
    edges: [Edge];
    metadata: Metadata;
}
```

**VM Versiyon Uyumluluk Matrisi:**

| IR Version | Compiler | VM v1.x | VM v2.x | VM v3.x |
|------------|----------|---------|---------|---------|
| 1 (v1.0) | Erken MVP | ✅ Full | ✅ Full | ✅ Full |
| 2 (v2.0) | FlatBuffers backend | ⚠️ Kısmi (string ID'ler) | ✅ Full | ✅ Full |
| 3 (v3.0) | +Suspend, +Retry opcode | ❌ UnknownOpcode | ❌ UnknownOpcode | ✅ Full |

**VM yükleme protokolü:**

```rust
fn load_plan(binary_blob: &[u8]) -> Result<ExecutionPlan> {
    let plan = flatbuffers::root::<ExecutionPlan>(binary_blob)?;
    
    let ir_version = plan.version();
    let vm_version = env!("CARGO_PKG_VERSION_MAJOR");  // runtime VM major versiyonu
    
    match (vm_version, ir_version) {
        // ✅ Full compatibility
        (3, 1) | (3, 2) | (3, 3) => Ok(plan),
        (2, 1) | (2, 2) => Ok(plan),
        (1, 1) => Ok(plan),
        
        // ⚠️ Partial: eski VM yeni IR'i bilmediği opcode'lar içerebilir
        (2, 3) => {
            // v2 VM, v3 IR'i yüklüyor — bilinmeyen opcode'lar olabilir
            warn!("IR v3 loaded on VM v2: new opcodes (Suspend=12) will fail");
            Ok(plan)  // Yine de yüklenir, bilinmeyen opcode'da UnknownOpcode hatası döner
        }
        (1, 2) | (1, 3) => {
            warn!("IR v{} loaded on VM v1: only string IDs supported", ir_version);
            Ok(plan)
        }
        
        // ❌ Incompatible
        _ => Err(GraphError::IncompatibleVersion {
            ir_version,
            vm_version,
            message: format!("IR v{} requires VM v{} or higher", ir_version, vm_version)
        })
    }
}
```

**Örnek — geriye dönük uyumluluk senaryosu:**

```
2026-07-01: Graph compile edilir → IR v2 (FlatBuffers, uint32 indexes)
2026-10-01: VM v3'e yükseltilir (Suspend, Retry opcode'ları eklendi)
2026-10-01: IR v2, VM v3'te çalışır → ✅ Full compatibility
            (VM v3, Opcode::Input=0..Error=10'u bilir, Suspend=12'yi IR v2'de görmez)
2026-11-01: Graph yeniden compile edilir → IR v3 (Suspend kullanır)
2026-11-01: IR v3, VM v2'de çalıştırılmaya çalışılır → ⚠️ Suspend opcode'u UnknownOpcode hatası
            Çözüm: VM yükselt veya graph'ı Suspend kullanmadan yeniden compile et
```

**Yeni opcode ekleme prosedürü (tüm proje için):**

```
1. Opcode enum'una yeni variant ekle (sona, explicit value ile)
   → Opcode::Suspend = 12
2. FlatBuffers schema field ID'lerini güncelleme (zaten @0..@N şeklinde)
3. VM'de match arm'ı ekle: Opcode::Suspend => { ... }
4. Eski VM'ler bu opcode'u bilmez → UnknownOpcode hatası döner (beklenen davranış)
5. Compiler'da yeni opcode için codegen ekle
6. IR version'ı artır (version: ushort = 3)
7. VM Version Compatibility Matrix'i güncelle
8. Test: eski IR'ler yeni VM'de çalışır mı? (birim test)
   Test: yeni IR eski VM'de çalışırsa UnknownOpcode hatası döner mi? (birim test)
```

#### 4.4.3 Tool Version Pinning

FlatBuffers IR, tool adının yanında **semver constraint** de saklar (`ToolDep` table). Bu sayede:

- Graph v1, `arac_sorgu@^1.0.0` ile compile edildi → v2.0.0 breaking change gelirse graph hâlâ v1.x ile çalışır
- Engine runtime'da Dispatch ederken uyumlu tool versiyonunu seçer
- Tool Registry'de aynı anda birden çok majör versiyon bulunabilir

```fbs
table ToolDep {
    name: string;        // "arac_sorgu" 
    version: string;     // "^1.0.0" (semver constraint)
    pure: bool;
}
```

**Tool Registry'de versiyonlama:**

```json
{
  "tools": [
    {
      "name": "arac_sorgu",
      "version": "2.1.0",
      "available_versions": ["1.0.0", "2.0.0", "2.1.0"],
      "breaking_changes": {
        "2.0.0": "Yeni zorunlu input: 'sigorta_turu' (string)"
      },
      ...
    }
  ]
}
```

**Engine dispatch kuralları:**

| IR'de pin | Registry'deki versiyon | Dispatch edilen |
|-----------|----------------------|-----------------|
| `^1.0.0` | v1.0.0, v1.2.0, v2.0.0 | v1.2.0 (en yüksek uyumlu) |
| `~1.0.0` | v1.0.0, v1.0.3, v2.0.0 | v1.0.3 |
| `=1.0.0` | v1.0.0, v1.2.0 | v1.0.0 (sadece exact) |
| `>=2.0.0` | v1.0.0, v2.1.0, v3.0.0 | v2.1.0 |
| Yok (eski IR) | herhangi | latest |

**Uyumlu versiyon bulunamazsa:**

```
VM: "Graph 'kasko_teklifi' v1 'arac_sorgu@^1.0.0' gerektiriyor.
    Registry'de sadece v2.0.0+ var. Graph yeniden compile edilmeli."
```

Bu sayede **tool geliştiricisi breaking change yapabilir**, ama mevcut graph'lar bozulmaz.

**Codegen çıktısı:** Bir `ExecutionPlan` binary blob'u. Boyutu:

| Graph büyüklüğü | JSON | FlatBuffers | Oran |
|-----------------|------|-------------|------|
| 10 node, 10 edge | ~1.5 KB | ~350 bytes | %23 |
| 50 node, 60 edge | ~8 KB | ~1.8 KB | %22 |
| 200 node, 250 edge | ~35 KB | ~7 KB | %20 |

### 4.5 Codegen Detayı: String Node ID → Uint32 Index

**Problem:** Opcode AST'de node ID'leri string'tir (`"giris"`, `"fiyat_hesapla"`, `"indirim_uygula"`). VM bu string'leri hash map'te aramak zorunda kalırsa O(log n) veya O(n) maliyeti oluşur.

**Çözüm — Codegen Pass (FlatBuffers Codegen aşamasında):**

```
Opcode AST (string ID'ler):          FlatBuffers IR (uint32 index'ler):
  Node: id="giris"                     Node[0]: id="giris", op=Input
  Node: id="fiyat_hesapla"             Node[1]: id="fiyat_hesapla", op=Call
  Node: id="indirim_uygula"            Node[2]: id="indirim_uygula", op=Calc
  Edge: from="giris" → to="fiyat_hesapla"    Edge[0]: from=0, to=1
  Edge: from="fiyat_hesapla" → to="indirim_uygula"   Edge[1]: from=1, to=2
```

**Codegen pass (Codegen aşaması, algoritma):**

```
1. Opcode AST'den tüm node'ları topla
2. Her node'a bir uint32 index ata (topolojik sırayla veya insertion order)
3. Bir `HashMap<String, u32>` oluştur: node_id → index
4. Her edge'in from/to değerini bu map'ten index'e çevir
5. DECIDE/SWITCH node'larının true_id/false_id/default_id değerlerini index'e çevir
6. FlatBuffers schema'yı Edge/Node için uint32 kullanacak şekilde güncelle
```

**Güncellenmiş FlatBuffers schema:**

```fbs
table Node {
    id: string;            // İnsan okunabilir ID, sadece debug/audit için (boş olabilir)
                           // VM kullanmaz, sadece execution_steps'te log için
    op: Opcode;            // enum, 1 byte
    args: [Arg];           // key-value çiftleri
    output_name: string;   // context key (boş olabilir)
}

table Edge {
    from_index: uint;      // uint32 index (Node tablosundaki offset)
    to_index: uint;        // uint32 index
    condition: string;     // boş string = unconditional
}

// DECIDE/SWITCH gibi çoklu çıkışı olan node'lar için ayrı bir tablo:
table BranchTarget {
    condition_value: string;  // "true" / "false" veya switch case değeri
    target_index: uint;       // hedef node'un index'i
}
```

**VM'de O(1) erişim:**

```rust
// VM — zero-copy, O(1) node access
let plan = flatbuffers::root::<ExecutionPlan>(&binary_blob)?;
let nodes = plan.nodes();  // FlatBuffers vector (contiguous in memory)

// O(1) — direkt index ile node'a eriş
let node = nodes.get(current_index);  // HashMap yok, offset table lookup

// Edge traversal:
let edge = plan.edges().get(some_edge_index);
let next_node = nodes.get(edge.to_index());  // O(1)

// DECIDE: condition_value ile BranchTarget'ı bul, target_index'e git
// BranchTarget'lar küçük olduğu için linear scan yeterli (en fazla 2-20 target)
// Alternatif: HashMap<String, u32> — compile-time oluştur, VM'de O(1)
```

**Boyut karşılaştırması (string vs uint32):**

| Bileşen | String ID (UTF-8) | Uint32 Index |
|---------|-------------------|-------------|
| Node reference (edge) | ~10-20 bayt | 4 bayt |
| DECIDE target | ~10-20 bayt × 2 | 4 bayt × 2 |
| 200 node graph, 250 edge | ~7 KB | ~2.5 KB |
| 500 node graph, 600 edge | ~35 KB | ~8 KB |

**Codegen Geçiş Stratejisi:**

```
v1.0 (MVP): String ID kullan (JSON interpreter, FlatBuffers yok)
            → Edge: from/to string, Node: id string
            → VM O(n) linear scan (≤50 node için yeterli)

v2.0 (Production): Uint32 Index kullan (FlatBuffers Codegen)
                  → Codegen pass: string → uint32 dönüşümü
                  → Edge: from_index/to_index uint32
                  → VM O(1) random access
                  → Node.id string hâlâ debug/audit için tutulur (opsiyonel, boş olabilir)
```

### 4.6 Zero-Copy Execution Model

VM, FlatBuffers IR'i şu şekilde kullanır:

```rust
// VM'de zero-copy okuma — allocation yok
let plan = flatbuffers::root::<ExecutionPlan>(&binary_blob)?;

// Node'a O(1) erişim: nodes tablosu offset array
let node = plan.nodes().get(node_index);  // HashMap yok, direkt pointer

// Opcode'u oku
match node.op() {
    Opcode::Call => {
        let target = node.args().get(0).value();  // zero-copy string view
        // ...
    }
    Opcode::Decide => {
        let source = node.args().get_by_key("source");  // O(log n)
        // ...
    }
}
```

**VM'in JSON'dan FlatBuffers'a geçişte kazandıkları:**

| Operasyon | JSON Interpreter | FlatBuffers VM |
|-----------|-----------------|----------------|
| Plan yükleme | ~50µs (parse + alloc) | ~0.5µs (mmap + root pointer) |
| Node'a ID ile erişim | O(n) linear scan veya hash | O(1) offset table |
| Opcode okuma | string compare | 1 byte enum read |
| Arg değer okuma | JSON re-parse | string view (pointer + length) |
| Memory | JSON tree (heap) | Buffer view (stack) |
| Cache locality | Düşük (heap fragmentation) | Yüksek (sequential buffer) |

### 4.7 Python Code Persistence

FlatBuffers IR sadece VM içindir. Kullanıcıya/LLM'ye gösterilen ve SQLite'da saklanan format **orijinal Python kodudur** (Section 5):

```
LLM/User: Restricted Python Code
                     ↘
                  Compiler
                     ↓
    Opcode AST ──→ FlatBuffers IR (production)
      ↘
    Python Code → SQLite'a definition olarak kaydedilir (original code)
```

Bu sayede:
- **Audit:** "Bu graph hangi kodla oluşturuldu?" sorusuna Python kod ile cevap verilir
- **Fork/Update:** Branch explore'da orijinal Python kod alınır, LLM üzerinde değişiklik yapar
- **Debug:** Execution hatasında Python kod satırı referans gösterilir

### 4.8 Restricted Python Specification

LLM, sıfırdan bir DSL yerine **kısıtlanmış Python (Restricted Python)** üretir. Python, LLM'lerin en yetenekli olduğu dildir ve `rustpython_parser` ile parse edilir. Ardından AST Sanitizer (Bölüm 4.2) güvenlik katmanından geçer.

#### 4.8.1 Allowed Python Constructs

| Construct | Python Syntax | Opcode | Açıklama |
|-----------|--------------|--------|----------|
| **Function def** | `def graph(plaka, yas):` | INPUT | Sadece `graph` adında tek fonksiyon |
| **Type hints** | `def graph(plaka: str, yas: int):` | INPUT | Input tipleri type hint ile belirtilir |
| **Call** | `arac = call("tool", plaka=plaka)` | CALL | Sadece tool/subgraph fonksiyonları |
| **Act** | `act("TEKLIF_SUN", teklif=result)` | ACT | Yan etki oluşturan işlem |
| **Arithmetic** | `fiyat = deger * 0.05 + 10` | CALC | Standart Python matematik operatörleri |
| **If/elif/else** | `if yas < 25: ... else: ...` | DECIDE | Koşullu dallanma |
| **For** | `for i in range(len(items)):` | LOOP | Sınırlı döngü (`while` yasak) |
| **Parallel** | `with parallel() as p: p.run(f1); p.run(f2)` | PARALLEL | Context manager ile paralel çalıştırma |
| **Return** | `return {"teklif": fiyat}` | OUTPUT | Graph çıktısı |
| **Attribute** | `arac.marka` | — | Nested field erişimi (dict) |
| **Dict literal** | `{"fiyat": 100, "onay": True}` | — | Sabit veri yapıları |
| **List literal** | `[1, 2, 3]` | — | Liste sabitleri |
| **Boolean ops** | `and`, `or`, `not` | — | Mantıksal operatörler |
| **Comparison** | `==`, `!=`, `<`, `<=`, `>`, `>=` | — | Karşılaştırma operatörleri |

#### 4.8.2 Forbidden Python Constructs (Sanitizer Tarafından Engellenir)

```python
import os                        # ❌ Yasak: Import
from tools import *              # ❌ Yasak: ImportFrom
eval("os.system('rm -rf /')")    # ❌ Yasak: eval/exec
class Hesaplama:                 # ❌ Yasak: ClassDef
while True:                      # ❌ Yasak: While (sonsuz döngü riski)
lambda x: x + 1                  # ❌ Yasak: Lambda
[x * 2 for x in items]           # ❌ Yasak: Comprehension
with open("/etc/passwd"):        # ❌ Yasak: With blokları
try: ... except: ...             # ❌ Yasak: Try/except (graph'in ERROR kullan)
async def graph():               # ❌ Yasak: Async
f"value is {x}"                  # ❌ Yasak: F-string (template syntax kullan)
x := 5                          # ❌ Yasak: Walrus operator
```

#### 4.8.3 Python → Opcode AST Mapping

| Python Construct | Transform → | Opcode Node |
|-----------------|-------------|-------------|
| `def graph(a: str, b: int):` | → | INPUT(a: string), INPUT(b: int) |
| `x = call("tool", ...)` | → | CALL(tool, params, output=x) |
| `x = a + b * 2` | → | CALC(expr, output=x) |
| `if cond: ... else: ...` | → | DECIDE(cond) + edges |
| `for i in range(N): body` | → | LOOP(body, max=N) |
| `with parallel() as p:` | → | PARALLEL(branches) |
| `act("TYPE", ...)` | → | ACT(type, params) |
| `return value` | → | OUTPUT edge |

#### 4.8.4 Context Referansları

Python değişken isimleri context key'lerine karşılık gelir:

```python
def graph(plaka: str, yas: int):
    # "plaka" ve "yas" → INPUT node'ları → context["plaka"], context["yas"]
    
    arac = call("arac_sorgu", plaka=plaka)
    # "arac" → CALL output → context["arac"] = {"marka": "Toyota", ...}
    
    model = arac.marka
    # "arac.marka" → attribute erişim → context["arac"]["marka"]
    
    # Template syntax (sadece act içinde):
    act("TEKLIF_SUN", mesaj="Araciniz: {{arac.marka}} {{arac.model}}")
```

#### 4.8.5 Tam Örnek

```python
def graph(plaka: str, yas: int, bolge: str):
    # STEP 1: Araç sorgulama
    arac = call("arac_sorgu", plaka=plaka)
    
    # STEP 2: Yaş bazlı fiyatlandırma
    if yas < 25:
        risk = call("risk_skoru", yas=yas, model=arac.marka)
        fiyat = arac.deger * 0.05 + risk.puan * 10
        fiyat = fiyat * 1.10
        act("GENCLIK_INDIRIMI", mesaj="%10 ek prim uygulandi")
    else:
        fiyat = arac.deger * 0.05 * 1.18
    
    # STEP 3: Bölgesel indirim ve kampanya (paralel)
    with parallel() as p:
        indirim = p.run(call, "bolgesel_indirim", bolge=bolge)
        kampanya = p.run(call, "aktif_kampanyalar", yas=yas)
    
    # STEP 4: Nihai hesaplama
    son_fiyat = fiyat - indirim.tutar + kampanya.ek_puan
    
    # STEP 5: Sonuç
    act("TEKLIF_SUN", teklif=son_fiyat, plaka=plaka)
    return {"teklif": son_fiyat, "plaka": plaka}
```

**For loop ile iterasyon örneği (LOOP):**

```python
def toplu_islem(dosya: str):
    satirlar = call("dosya_oku", path=dosya)
    
    for i in range(len(satirlar)):
        kayit = call("kayit_oku", index=i)
        sonuc = call("islem_yap", veri=kayit)
        act("LOG_KAYDET", seviye="info", mesaj=f"Islem {i+1} basarili")
    
    return {"islenen": len(satirlar)}
```

**Neden bu Python kodu geçerli?**
- Sadece `def`, `call()`, `act()`, `if`, `for`, `return`, matematik, karşılaştırma kullanır
- `import` yok, `class` yok, `while` yok, `eval` yok
- Tüm değişkenler context'te saklanır, Python scope kuralları geçerli değil
- `for` döngüsü `range()` ile sınırlandırılmıştır, compiler tarafından LOOP opcode'una dönüştürülür
- `with parallel()` context manager PARALLEL opcode'una dönüştürülür

#### 4.8.6 Auto-Repair Loop Detail

Compiler, Python kodunu parse ederken (rustpython_parser) veya valide ederken (Sanitizer + Static Validation) hata bulursa, hatayı **insan tarafından okunabilir formatta** LLM'e döndürür:

```
━━━ Compiler Feedback ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  HATA (5, 12): Tool 'mail_gonder' bulunamadı.
  Beklenen: Tool Registry'de kayıtlı bir tool adı.
  Kullanılabilir: ['arac_sorgu', 'fiyatlandir', 'risk_skoru']
  
  Kodun 5. satırı:
    5   │   mail = call("mail_gonder", to=email)
        │            ^~~~~~~~~~~~~~ burada hata
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

LLM: "Özür dilerim, 'mail_gonder' henüz kayıtlı değil.
      İşlemi log kaydına dönüştürüyorum:"

    5   │   act("LOG_KAYDET", seviye="bilgi", mesaj="Mail gonderilemedi")
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ✅ Başarılı: Graph "kasko_teklifi" valide edildi.
  Node sayısı: 12, Edge sayısı: 14
  Optimizasyonlar: constant_folding, dead_node_elimination
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

Bu döngü, LLM'in sonsuz JSON bracket kapatma mücadelesi yerine **gerçek mantık hatalarına odaklanmasını** sağlar. LLM'in Python syntax hatası yapması çok nadir olduğu için auto-repair döngüsü neredeyse her zaman validation/sanitization hatalarına odaklanır.

---

## 5. LLM ile Graph Oluşturma

### 5.1 Yöntem

**Sohbet + Python Code.** LLM önce iş birimiyle konuşarak ihtiyacı anlar, sonra tek tool çağrısıyla **Restricted Python kodu** üretir. İş birimi onaylayana kadar sohbet devam eder.

```
İş birimi: "30 yaş altına %10 indirim yapan kasko ürünü"
   ↓
LLM: "Anladım. 25 yaş sınırı olsun, değer limiti 1 milyon?"
   ↓
İş birimi: "Evet, doğru"
   ↓
LLM: create_graph(code="...")   ← Restricted Python kodu üretir
   ↓
Compiler: Validasyon → "5. satırda hata: tool 'risk_skoru' bulunamadı"
   ↓
LLM: "Özür dilerim, düzeltiyorum" → update_graph(code="...")
   ↓
Compiler: ✅ Başarılı → Graph draft olarak kaydedilir
```

**Neden Restricted Python, JSON değil?**
- LLM token'larının ~%60'ı JSON syntax'ına (süslü parantez, tırnak, virgül) harcanır
- JSON hataları (unclosed bracket, wrong enum) LLM'in en zayıf olduğu alandır
- Python'da LLM mantığa odaklanır, syntax'ı `rustpython_parser` halleder
- Hata durumunda compiler satır numarası söyler, LLM kendi kodunu düzeltir
- Python, JSON'dan sadece ~%10 daha fazla token harcar, ama milyonlarca satır test edilmiş parser ile sıfır syntax hatası

### 5.2 LLM Tool'ları (4 adet)

| Tool | Ne yapar |
|------|----------|
| `create_graph(name: string, code: string)` | Yeni graph oluşturur. Restricted Python kodunu alır, `rustpython_parser` ile parse eder, Sanitizer + Validator'dan geçirir, draft olarak kaydeder. Hata varsa satır numarası + sebep döndürür. |
| `update_graph(id: string, code: string)` | Varolan graph'ın Python kodunu günceller. **Immutable model:** yeni versiyon oluşturur (Bölüm 10). |
| `fork_graph(id: string, fork_node: string, code: string)` | Varolan graph'ı fork eder. Yeni branch'in Python kodunu alır. |
| `deploy_graph(id: string)` | Graph'ı production'a alır. Validasyon geçmiş olmalıdır. Deploy = pointer'ı yeni versiyona çevir. |

### 5.3 Graph Generation Stratejisi

**Basit akışlar (< 20 node / < 50 satır kod):** LLM tüm graph'ı tek seferde üretir (create_graph).

**Kompleks akışlar (20-50 node / 50-200 satır):** Incremental. LLM her turda 5-15 satır kod ekler, iş birimi onaylar, devam eder.

**Çok kompleks akışlar (> 50 node / > 200 satır):** Subgraph'lara bölünür. Her subgraph bağımsız bir `graph` tanımıdır, kendi testi yapılabilir, başka graph'lar tarafından `call("subgraph:adi")` ile çağrılır.

### 5.4 Auto-Repair Loop

Compiler validasyon hatası döndürdüğünde, LLM kendi kodunu düzeltir:

```
create_graph("kasko_teklifi", code="...")
   ↓
Compiler: "HATA (8, 15): 'risk_skoru' tool'u bulunamadı.
           Kullanılabilir: ['arac_sorgu', 'fiyatlandir']"
   ↓
LLM: "Tool adını yanlış yazmışım. Düzeltiyorum:"
update_graph("g_abc123", code="...")   ← düzeltilmiş kod
   ↓
Compiler: "✅ Başarılı. 12 node, 14 edge. Graph draft."
```

Bu döngü en fazla 2-3 iterasyonda tamamlanır. İş birimi bu süreci görmez — LLM arka planda kendi kodunu düzeltir.

### 5.4 Tool Registry (LLM Tarafından Okunur)

LLM graph oluşturmadan önce Tool Registry'yi okur:

```json
{
  "tools": [
    {
      "name": "arac_sorgu",
      "description": "Plaka ile araç bilgilerini sorgular",
      "version": "2.1.0",
      "owner": "sigorta-ekibi",
      "tags": ["arac", "sigorta", "sorgu"],
      "permissions": ["read:arac"],
      "pure": true,
      "input_schema": {
        "type": "object",
        "properties": {
          "plaka": {"type": "string", "pattern": "^\\d{2}[A-Z]{1,3}\\d{2,4}$"}
        },
        "required": ["plaka"]
      },
      "output_schema": {
        "type": "object",
        "properties": {
          "marka": {"type": "string"},
          "model": {"type": "string"},
          "yil": {"type": "integer"}
        }
      },
      "examples": [
        {"input": {"plaka": "34ABC123"}, "output": {"marka": "Toyota", "model": "Corolla", "yil": 2024}}
      ]
    },
    {
      "name": "mail_gonder",
      "description": "E-posta gönderir",
      "version": "1.0.0",
      "owner": "sigorta-ekibi",
      "tags": ["bildirim", "mail"],
      "permissions": ["write:mail"],
      "pure": false,
      "input_schema": { ... }
    }
  ]
}
```

---

### 5.5 LLM Sorumluluk Dağılımı (Kim Ne Yapıyor)

```
İş birimi (sohbet)
    │
    ▼
TinyOS (agent loop + channels)                          ← TinyOS
    │  LLM'yi çağırır (tinyos-providers)
    │  Kullanıcıyla sohbet eder
    │  "Şu graph'ı oluştur" → LLM'ye gönderir
    ▼
LLM → Restricted Python code döner
    │
    ▼
tinypipe-compiler                                       ← tinypipe
    │  Python'ı parse et (rustpython-parser)
    │  AST Sanitize et
    │  Opcode AST'ye çevir
    │  Validasyon
    │  Hata varsa → TinyOS'a döner
    ▼
Hata mı var? ──yes──▶ TinyOS, LLM'e feedback gönderir  ← TinyOS (auto-repair)
    │
    no
    ▼
tinypipe-vm                                             ← tinypipe
    │  DAG interpreter (deterministik, LLM yok)
    │  Her CALL'da ToolRegistry::dispatch çağırır
    ▼
ToolRegistry::dispatch(call)                            ← TinyOS implemente eder
    │  (TinyOsToolRegistry)
    │  tinymachine SandboxBackend'i çağırır
    ▼
tinymachine-fork                                        ← tinymachine (zaten var)
    KVM/wasm ile kod çalıştır
```

| Bileşen | Kim yazıyor? | Sorumluluk |
|---------|-------------|------------|
| **tinypipe** (6 crate) | Bu proje | Restricted Python parse + sanitize + validate + DAG interpreter |
| **TinyOS → LLM entegrasyonu** | TinyOS | LLM çağrısı, sohbet UI, auto-repair loop, deploy |
| **TinyOsToolRegistry** | TinyOS | `ToolRegistry` trait'ini implemente et, tinymachine'e bağla |
| **tinymachine** | Zaten var | KVM fork / wasm ile kod çalıştırma |

**Önemli:** tinypipe LLM'i çağırmaz, tinymachine'i tanımaz. Sadece:
1. Restricted Python kodu alır → parse/validate → ExecutionPlan üretir
2. ExecutionPlan alır → `ToolRegistry::dispatch` ile deterministik çalıştırır

LLM sohbeti, auto-repair, deploy — hepsi TinyOS'un sorumluluğudur.

---

## 6. Production Execution

### 6.1 Graph Engine / VM

Production'da LLM yoktur. Graph Engine (VM) **FlatBuffers IR** üzerinde zero-copy çalışan bir DAG interpreter'dır:

1. **Plan yükleme:** DB'den FlatBuffers binary blob okunur, `flatbuffers::root::<ExecutionPlan>()` ile zero-copy pointer alınır (~0.5µs, allocation yok)
2. **Context başlatılır** (müşteri input'u ile doldurulur)
3. **Topolojik sırayla** her node çalıştırılır — Node'a `get(node_index)` ile O(1) erişim, opcode 1 byte enum read
4. **Her node:** context'ten oku → işle → context'e yaz
5. Her node'dan sonra **snapshot** alınır (execution_steps'e yazılır)
6. PARALLEL: Her branch için **Local Scope Memory** (context kopyası) oluştur, thread/async pool'da eşzamanlı çalıştır, MERGE'de bekle — race condition yok (her branch kendi kopyasında yazar)
7. LOOP: condition sağlanana kadar body'yi tekrarla, max_iterations aşılınca dur
8. WAIT: timer/schedule mekanizması
9. ERROR: hata yakala, error_types eşleşirse fallback node'a git
10. CALL: target tipine göre (tool/subgraph/rpc/plugin) dispatch et
11. Sonuç: ACT çıktıları + son context

**Execution budget:** Her graph'ın `Metadata.max_node_execution_count` (varsayılan: 10.000) değeri vardır. VM her node execution'da counter'ı artırır, limit aşılırsa execution'ı durdurur:

```rust
// VM'de execution budget kontrolü
fn execute_plan(plan: &ExecutionPlan) -> Result<Context> {
    let budget = plan.metadata().max_node_execution_count();
    let mut counter = 0u32;
    
    while let Some(node) = next_node() {
        counter += 1;
        if counter > budget {
            return Err(GraphError::ExecutionBudgetExceeded(
                format!("Execution budget ({}) exceeded at node {}", budget, node.id())
            ));
        }
        execute_node(node)?;
    }
    Ok(context)
}
```

**Context memory limit:** Her graph'ın `Metadata.max_context_memory_bytes` (varsayılan: 10 MB) değeri context/heap boyutunu sınırlar. Bir tool aşırı büyük JSON döndürürse VM OOM olmaz:

```rust
// VM'de context memory limit kontrolü
fn execute_node(node: &Node, context: &mut Context) -> Result<()> {
    let limit = plan.metadata().max_context_memory_bytes();
    
    // CALL node'u: tool output'u context'e yazılmadan önce boyut kontrolü
    if node.op() == Opcode::Call {
        let output = dispatch_tool(node)?;
        let output_size = estimate_json_size(&output);
        if context.estimated_bytes() + output_size > limit {
            return Err(GraphError::ContextMemoryExceeded(
                format!("Context memory limit ({}) exceeded by tool output ({} bytes)",
                    limit, output_size)
            ));
        }
        context.set(node.output_name(), output);
    }
    Ok(())
}
```

**Subgraph recursion limit:** CALL target `subgraph:adi` ile başka bir graph çağrıldığında, VM `max_recursion_depth` (varsayılan: 5) seviyesine kadar izin verir. Aşılınca execution durur:

```rust
// VM'de recursion depth kontrolü
struct VmState {
    depth: u32,          // mevcut subgraph çağrı derinliği
    max_depth: u32,      // plan.metadata().max_recursion_depth()
}

fn dispatch_subgraph(target: &str, state: &mut VmState) -> Result<Context> {
    if state.depth >= state.max_depth {
        return Err(GraphError::RecursionLimitExceeded(
            format!("Subgraph recursion depth ({}) exceeded at {}", state.max_depth, target)
        ));
    }
    state.depth += 1;
    let result = execute_subgraph(target)?;
    state.depth -= 1;
    Ok(result)
}
```

**VM FlatBuffers avantajı:** "JSON parse" diye bir adım yoktur. VM, `mmap` ile buffer'ı belleğe haritalar, root pointer alır, doğrudan node'lara ve edge'lere erişir. Bu sayede v2'de bile execution latency JSON interpreter'a göre ~10× daha düşüktür.

**Wall-clock timeout — instruction cycle budget:** Her graph'ın `Metadata.max_execution_time_ms` (varsayılan: 30.000ms = 30sn) değeri tüm execution için azami süreyi belirler. VM her node öncesi wall-clock süresini kontrol eder, limit aşılırsa execution'ı durdurur. Bu, `max_node_execution_count`'un kapatamadığı bir boşluğu kapatır: tek bir CALL tool'u 5 dakika sürse, node count artmaz ama timeout tetiklenir.

```rust
// VM'de wall-clock timeout kontrolü
struct VmBudget {
    node_count: u32,          // max_node_execution_count
    start_time: Instant,      // execution başlangıç zamanı
    max_time: Duration,       // max_execution_time_ms'den dönüştürülmüş
    max_nodes: u32,           // max_node_execution_count
    max_memory: u64,          // max_context_memory_bytes
    context_bytes: u64,       // mevcut context boyutu
}

impl VmBudget {
    fn check(&self, node: &Node) -> Result<()> {
        // 1. Node count budget
        if self.node_count > self.max_nodes {
            return Err(GraphError::ExecutionBudgetExceeded(...));
        }
        
        // 2. Wall-clock timeout
        if self.start_time.elapsed() > self.max_time {
            return Err(GraphError::ExecutionTimeoutExceeded(
                format!("Wall-clock timeout ({}ms) exceeded at node {}",
                    self.max_time.as_millis(), node.id())
            ));
        }
        
        // 3. Context memory limit
        if self.context_bytes > self.max_memory {
            return Err(GraphError::ContextMemoryExceeded(...));
        }
        
        Ok(())
    }
}

// Her node öncesi atomik kontrol:
fn execute_node(node: &Node, budget: &VmBudget, context: &mut Context) -> Result<()> {
    budget.check(node)?;  // Üç kontrol tek noktada

    if node.op() == Opcode::Call {
        let output = dispatch_tool(node)?;  // Tool 30sn timeout'a takılabilir
        // dispatch_tool içinde de ayrıca timeout kontrolü (http client timeout)
    }
    
    Ok(())
}
```

**Atomik kontrol noktaları (her node'da zorunlu):**
1. Node count budget — `counter > max_node_execution_count`
2. Wall-clock timeout — `start_time.elapsed() > max_execution_time_ms`
3. Context memory — `context.estimated_bytes() > max_context_memory_bytes`

Bu üç kontrol, VM'in her node öncesi tek bir `budget.check()` çağrısında toplanır. Kontrollerin **hepsi** deterministiktir — aynı input, aynı budget, aynı node sırası → her zaman aynı noktada timeout alınır.

**Timeout sonrası davranış:**
- Execution `failed` olarak işaretlenir
- O ana kadarki context snapshot'ı `execution_steps`'e kaydedilir (debug için)
- Hata mesajı: "Execution timeout (Xms) exceeded at node Y after Z nodes"
- Orchestrator, timeout alan execution'ı yeniden başlatabilir (opsiyonel, v2+)

### 6.2 Validasyon (İki Aşamalı)

#### Static Validation (deploy öncesi, compiler'da)

| Kontrol | Ne yapar | Hata örneği |
|---------|----------|-------------|
| **Cycle** | Graph DAG olmalı, cycle varsa reddet | "Node X ve Y arasında cycle var" |
| **Subgraph Cycle** | Global Call Graph DFS — tüm subgraph'lar arasında döngüsel bağımlılık olmamalı | "Subgraph cycle: A→B→C→A" |
| **Node varlığı** | Tüm edge referansları geçerli node ID'sine ait olmalı | "Edge X→Y: Y node'u bulunamadı" |
| **Tool varlığı** | Tüm CALL'lar kayıtlı tool'ları/subgraph'ları çağırmalı | "Tool 'arac_sorgu' bulunamadı" |
| **Subgraph nesting** | Subgraph iç içe geçme derinliği max_subgraph_depth (varsayılan: 5) aşmamalı | "Subgraph nesting depth (6) max (5)'ı aştı" |
| **Terminal** | Her dal bir ACT ile bitmeli | "Branch X terminal node değil" |
| **Input** | Graph'in gerektirdiği tüm input'lar tanımlanmış olmalı | "Input 'plaka' tanımlı değil" |

#### Runtime Validation (execution sırasında, VM'de)

| Kontrol | Ne yapar | Aksiyon |
|---------|----------|---------|
| **Timeout** | CALL timeout_ms aşılınca | Retry veya ERROR node'una git |
| **Retry** | CALL başarısız olursa max_attempts'e kadar dene | backoff_ms ile tekrar dene |
| **on_error** | CALL hata verdiğinde `on_error` parametresine göre davran | "abort" → ERROR node | "continue_with_null" → context'e null yaz | "continue_with_fallback" → fallback_value yaz |
| **Schema Validation** | ToolDep.schema_hash ile Registry'deki güncel şemayı karşılaştır | Şema değişmişse hata: "Tool schema changed, recompile required" |
| **Permission** | Tool'un gerektirdiği izinler var mı | CALL öncesi kontrol |
| **Quota** | Tool çağrı limiti aşıldı mı | Rate limit |
| **Context Memory** | context boyutu max_context_memory_bytes aştı mı | `ContextMemoryExceeded` hatası |
| **Recursion** | subgraph çağrı derinliği max_recursion_depth aştı mı | `RecursionLimitExceeded` hatası |
| **Circuit Breaker** | Tool sürekli hata veriyorsa aç | Fallback'e yönlendir |
| **Wall-Clock Timeout** | execution süresi `max_execution_time_ms` (varsayılan: 30sn) aştı mı | `ExecutionTimeoutExceeded` hatası — tüm execution durdurulur, context snapshot'ı kaydedilir |
| **Instruction Cycle Budget** | Her node execution sonrası wall-clock süresi kontrol edilir. CALL tool'ları da dahil — tool 10sn sürerse bile VM timeou'tu yakalar | Her node öncesi `Instant::now()` kontrolü: aşıldıysa node çalıştırılmaz, direkt hata dön |

### 6.3 CALL Target Tipleri

| Target Format | Açıklama | Örnek |
|---------------|----------|-------|
| `tool:arac_sorgu` | Kayıtlı tool'u çağır (latest) | `tool:arac_sorgu` |
| `tool:arac_sorgu@^1.0.0` | Kayıtlı tool'u semver constraint ile çağır | `tool:arac_sorgu@^1.0.0` |
| `tool:arac_sorgu@=2.1.0` | Exact versiyon | `tool:arac_sorgu@=2.1.0` |
| `subgraph:kasko_standart` | Başka bir graph'ı çalıştır | `subgraph:kasko_standart_v2` |
| `rpc:http://...` | HTTP API'yi çağır | `rpc:http://internal/pricing` |
| `plugin:hesap` | Yerleşik plugin | `plugin:hesap` |
| `llm:claude-sonnet` | LLM çağrısı (sadece design-time) | `llm:claude-sonnet` |

**Compiler ToolDep'a otomatik semver ekler:**
- Python'da `call("arac_sorgu", ...)` yazıldıysa → compiler IR'ye `tool:arac_sorgu@^2.1.0` yazar (Registry'deki mevcut versiyon)
- Python'da `call("arac_sorgu@=1.0.0", ...)` yazılırsa → compiler birebir kullanır, validasyon yapar

v1'de sadece `tool:` (semver'siz) ve `subgraph:` desteklenir.

**Version Drift senaryosu:**
```
1. Gün 1: Graph compile edilir → arac_sorgu@^1.0.0 pinlenir
2. Gün 30: Tool Registry'ye arac_sorgu@2.0.0 eklenir (breaking: yeni zorunlu input)
3. Gün 30: Graph çalışır → Engine Registry'den uyumlu versiyon arar
           → ^1.0.0 ile v1.5.0 eşleşir → v1.5.0 dispatch edilir
4. Gün 31: Graph yeniden compile edilir → arac_sorgu@^2.0.0 pinlenir
           → Yeni input'lar Python koduna eklenmiştir → çalışır
```

---

## 7. WAIT Stratejisi ve Durable Execution

### 7.1 WAIT'in Yarattığı Mimari Zorluk

Normal execution senkron çalışır: request gelir → 10-100ms içinde biter → response döner.
WAIT opcode'u bu akışı bozar:

| Senaryo | Süre | Sorun |
|---------|------|-------|
| `WAIT(10sn)` | 10 saniye | Thread 10sn bloke olur, havuzda yer işgal eder |
| `WAIT(3gün)` | 3 gün | Thread/process bekleyemez, kaynak tükenir |

İkinci senaryo için **Durable Execution** gerekir: execution'ı dondurup DB'ye yazmak, zamanı gelince uyandırmak.

### 7.2 v1 Stratejisi: Sınırlı WAIT (Senkron)

```
WAIT süresi ≤ 5 dk → Engine içinde async timer (in-memory)
WAIT süresi > 5 dk → Validasyon hatası: "v1'de >5dk WAIT desteklenmez"
```

- `max_wait_secs: 300` (5 dk üst limit)
- Async timer ile thread bloke edilmeden beklenir
- WAIT sırasında diğer execution'lar engine'de çalışmaya devam eder
- Bu süre zarfında process/crash olursa execution kaybolur (en fazla 5 dk'lık iş)

### 7.3 v2 Stratejisi: Durable Execution (Stateful WAIT)

5 dk üzeri WAIT'ler için durable execution:

```
1. Engine WAIT node'una gelir
2. Execution state DB'ye yazılır:
   - graph_id, paused_node, context (tüm state)
   - status: paused
   - scheduled_resume_at: timestamp (şimdi + duration)
3. Engine thread'i serbest kalır, başka execution alabilir
4. Scheduler (ayrı proses/thread) DB'yi periyodik sorgular
5. Zamanı gelen execution'ları engine'e geri gönderir
6. Engine DB'den state'i yükler, kaldığı yerden devam eder
```

### 7.4 Durable Execution İçin Gerekenler

| Bileşen | Ne işe yarar |
|---------|-------------|
| **DB'de paused state** | Execution'ın context'i + kaldığı node DB'ye yazılır |
| **scheduled_resume_at** | Hangi zamanda uyanacağı kaydedilir |
| **Scheduler** | Ayrı proses/thread, DB'yi periyodik sorgular, zamanı gelenleri uyandırır |
| **Engine'de resume** | DB'den state yükleme ve kaldığı node'dan devam etme yeteneği |
| **İdempotency** | Aynı execution birden çok uyandırılırsa sorun olmamalı |

### 7.5 Scheduler Detayı

Scheduler ayrı bir proses veya thread olarak çalışır:
- Periyodik: her 1sn'de bir DB sorgusu
- Zamanı gelen execution'ları `paused → running` yapar
- Engine'e `resume(execution_id)` sinyali gönderir
- Engine DB'den context'i okur, kaldığı node'dan devam eder
- Scheduler'ın kendisi de durable olmalı (crash'te kayıp yok)

TinyOS mimarisinde scheduler `tinyos-orchestrator` içinde yer alır.

### 7.6 SUSPEND/RESUME — Human-in-the-Loop

Bazı iş akışlarında **insan onayı** gerekir: bir teklif oluşturulur, yönetici onaylamalıdır, sonra devam edilir.

**Mevcut (7.3-7.5) Durable Execution WAIT için yeterli değildir** çünkü:
- WAIT belirli bir süre bekler, insan onayı ise **belirsiz süre** bekleyebilir (dakikalar, günler)
- WAIT scheduler periyodik sorgular, insan onayı **harici tetikleyici** (webhook, portal butonu) gerektirir
- Onay sırasında context + execution pointer'ı **güvenli şekilde** persist edilmelidir (process restart dayanıklı)

**Çözüm — SUSPEND opcode'u:** (yeni opcode, sona eklenir: `Suspend = 12`)

```
PARAMETRELER:
  suspend_type: string    // "human_approval" | "external_webhook" | "manual_review"
  description: string     // Onay mesajı: "Müşteriye 50.000 TL limitli teklif gönderilsin mi?"
  resume_webhook: string  // Opsiyonel: harici sistemden resume için webhook URL
  timeout_hours: uint     // Opsiyonel: max bekleme süresi (aşılınca ERROR)
  on_timeout: string      // "abort" | "continue_with_fallback"
  fallback_context: JSON  // timeout durumunda kullanılacak context patch
```

**Yaşam döngüsü:**

```
1. VM SUSPEND node'una gelir
2. Execution state DB'ye yazılır:
   - graph_id, paused_node, context (full state)
   - status: "awaiting_approval"
   - suspend_type, description, resume_webhook
3. Engine thread'i serbest kalır
4. Dış dünya (portal/buton/webhook) onay verene veya time out olana kadar beklenir
   
   Onay geldiğinde:
   a. Orchestrator `resume(execution_id)` API'sini çağırır
   b. Engine DB'den state'i yükler: context + paused_node
   c. Kaldığı node'un hemen sonrasından devam eder
   
   Timeout olduğunda:
   a. on_timeout == "abort" → execution failed olarak işaretlenir
   b. on_timeout == "continue_with_fallback" → fallback_context uygulanır, devam eder
```

**Human-in-the-loop örneği (Restricted Python):**

```python
def graph(musteri: dict, teklif: dict):
    # ... fiyat hesapla, risk puanı hesapla ...
    fiyat = call("fiyatlandir", ...)
    
    # Yüksek riskli müşteri: yönetici onayı gerekli
    if musteri.risk_puani > 80:
        suspend("human_approval",
                description=f"Müşteri {musteri.ad} için {fiyat} TL teklif onayı",
                timeout_hours=48,
                on_timeout="abort")
    
    # Yönetici onayladıysa veya risk düşükse devam
    act("TEKLIF_SUN", teklif=fiyat, musteri=musteri)
```

**SUSPEND state machine:**

```
RUNNING → (SUSPEND) → AWAITING_APPROVAL → (resume API) → RUNNING
                       AWAITING_APPROVAL → (webhook)    → RUNNING
                       AWAITING_APPROVAL → (timeout)    → FAILED / RUNNING (fallback)
```

**Storage (executions tablosuna yeni alanlar):**

| Alan | Tür | Açıklama |
|------|-----|----------|
| suspend_type | TEXT | null / "human_approval" / "external_webhook" |
| suspend_description | TEXT | Onay mesajı |
| resume_webhook | TEXT | Harici resume URL (opsiyonel) |
| suspend_timeout_at | TEXT | Timeout timestamp |
| paused_context | TEXT (JSON) | SUSPEND anındaki context snapshot'ı |

**Güvenlik ve idempotency:**
- Her execution'ın benzersiz `resume_token`'ı vardır (UUID), resume API'sinde bu token istenir
- Token 1 kere kullanılır: resume sonrası token geçersiz olur
- Aynı execution birden çok resume isteği alırsa → idempotency check (status != "awaiting_approval" ise ignore)

---

## 8. Branch Explore

Varolan bir graph'ın belirli bir noktasından alternatif senaryo oluşturma:

1. Graph fork noktasına kadar replay edilir
2. Belirtilen değer override edilir ("değer < 500K")
3. LLM bu yeni durumda ne yapacağını belirler
4. Yeni node'lar eklenir, yeni graph branch'i kaydedilir
5. Eski branch bozulmaz, tree'de kalır

Branch'ler parent-child ilişkisiyle tree yapısı oluşturur:

```
main (kasko_standart)
  ├── branch-a (yaş<25, fork_at=giris)
  ├── branch-b (değer>1M, fork_at=hesapla)
  │   └── branch-b1 (hasar_yok, fork_at=karar)
  └── branch-c (bölge=ege, fork_at=giris)
```

Her branch bağımsız bir graph'tır, kendi yaşam döngüsü vardır (draft → test → active → deprecated).

---

## 9. Storage (SQLite)

### 9.1 Graphs

| Alan | Tür | Açıklama |
|------|-----|----------|
| id | TEXT (UUID) | Birincil anahtar |
| name | TEXT | Ürün adı ("Genç Sürücü Kaskosu") |
| status | TEXT | draft / active / deprecated |
| version | INTEGER | Versiyon numarası (immutable, her update yeni versiyon) |
| active | BOOLEAN | Deploy edilmiş mi? (pointer) |
| parent_id | TEXT (UUID) | Branch tree için parent graph |
| fork_node | TEXT | Hangi node'dan fork edildi (string ID) |
| fork_label | TEXT | Fork sebebi ("yaş<25") |
| code | TEXT (Python) | Restricted Python kodu — LLM'in ürettiği orijinal kod, audit için saklanır |
| definition | TEXT (JSON) | Graph'in kendisi (nodes + edges) — v1'de doldurulur, v2'de code alanından derive edilir |
| execution_plan | BLOB (FlatBuffers) | Compiler çıktısı — binary bytecode (v2, null = henüz compile edilmemiş) |
| compiled_at | TEXT | Son compile zamanı (v2) |
| created_at | TEXT | Oluşturulma zamanı |
| updated_at | TEXT | Güncellenme zamanı |

### 9.2 Executions

| Alan | Tür | Açıklama |
|------|-----|----------|
| id | TEXT (UUID) | Birincil anahtar |
| graph_id | TEXT (UUID) | Hangi graph çalıştı |
| graph_version | INTEGER | Hangi versiyon |
| input | TEXT (JSON) | Müşteriden gelen veri |
| output | TEXT (JSON) | ACT node'larının çıktısı |
| status | TEXT | running / paused / completed / failed |
| error | TEXT | Varsa hata mesajı |
| started_at | TEXT | Başlangıç zamanı |
| completed_at | TEXT | Bitiş zamanı |
| duration_us | INTEGER | Toplam süre (mikrosaniye) |
| context | TEXT (JSON) | Execution sonundaki context |

### 9.3 Execution Steps (Per-Node Snapshot)

Her node'dan sonra snapshot alınır:

| Alan | Tür | Açıklama |
|------|-----|----------|
| id | TEXT (UUID) | Birincil anahtar |
| execution_id | TEXT (UUID) | Hangi execution'a ait |
| node_id | TEXT | Graph içindeki node id ("giris", "hesapla") |
| node_type | TEXT | INPUT / CALL / CALC / ... |
| node_name | TEXT | Node adı ("arac_sorgula") |
| context_before | TEXT (JSON) | Bu node başlamadan önceki context |
| context_after | TEXT (JSON) | Bu node bittikten sonraki context |
| status | TEXT | running / ok / failed / skipped |
| error | TEXT | Varsa hata mesajı |
| started_at | TEXT | Başlangıç zamanı |
| completed_at | TEXT | Bitiş zamanı |
| duration_us | INTEGER | Süre |
| parent_step_id | TEXT (UUID) | LOOP içindeki child step'ler için |

### 9.4 İlişkiler

```
graphs
  │
  ├── parent_id ──▶ graphs (branch tree)
  │
  └──▶ executions
         │
         └──▶ execution_steps
                │
                └── parent_step_id ──▶ execution_steps (LOOP child)
```

---

## 10. Immutable Graph + Pointer Deploy

Her `update_graph` yeni bir graph versiyonu oluşturur (mevcut graph değişmez):

```
create_graph("kasko_standart")  → Graph v1
update_graph("kasko_standart")  → Graph v2 (v1 immutable kalır)
update_graph("kasko_standart")  → Graph v3 (v1, v2 immutable kalır)
deploy_graph(v3)                → "kasko_standart" pointer'ı v3'e çevrilir
deploy_graph(v2)                → Rollback! Pointer v2'ye döner
```

**Avantajları:**
- Rollback: `deploy_graph(v2)` ile eski versiyona anında dönüş
- Audit: "v1'de 1000 execution, v2'de 500, v3'te 200" — her versiyonun execution trace'i ayrı
- A/B test: Farklı versiyonlar aynı anda aktif olabilir (kademeli rollout)
- Hiçbir graph silinmez: Sadece `deprecated` olur

---

## 11. Edge Metadata (v2)

Edge'ler sadece `from`/`to` değil, metadata da taşır:

```json
{
  "from": "fiyat_hesapla",
  "to": "indirim_uygula",
  "condition": "$yasin < 25",
  "priority": 1,
  "label": "genc_indirim",
  "mapping": {
    "fiyat": "indirimli_fiyat"
  }
}
```

| Alan | Açıklama |
|------|----------|
| `condition` | Bu edge'in aktif olması için koşul (opsiyonel) |
| `priority` | Birden çok edge arasında sıralama (opsiyonel) |
| `label` | Edge'in anlamı ("genc_indirim", "standart") |
| `mapping` | Context field'larını rename/map etme (opsiyonel) |

**v2'de:** Edge'lerdeki `condition` ile DECIDE opcode'unun `condition`'ı birleşebilir. DECIDE node'unun `true_id`/`false_id`'si yerine edge'lerde `condition` kullanılabilir.

---

## 12. Compensation / Saga (v3)

ACT yan etki oluşturduğunda (mail gönder, DB yaz, CRM kaydet) başka bir ACT başarısız olursa ne olur?

**v3'te Compensation:**

```json
{
  "id": "mail_gonder",
  "op": "ACT",
  "args": {
    "action_type": "notify",
    "content": "Poliçeniz oluşturuldu",
    "compensate": "tool:mail_iptal"
  }
}
```

- Her ACT opsiyonel bir `compensate` tool'u taşıyabilir
- Bir sonraki ACT başarısız olursa, önceki ACT'lerin compensate tool'ları ters sırada çağrılır
- Saga pattern: eventual consistency + compensating transactions

---

## 13. Typed Variables (v2)

Context'teki değerler tip taşır:

```json
{
  "variables": {
    "yasin": {"type": "int", "value": 25},
    "plaka": {"type": "string", "value": "34ABC123"},
    "musteri": {"type": "Customer", "value": {"id": "123", "ad": "Ali"}}
  }
}
```

**Avantajları:**
- **Compile-time validation:** `$yasin > 1M` → tip hatası (int > string karşılaştırılamaz)
- **Schema uyumu:** CALL output'u otomatik type cast
- **LLM daha iyi graph üretir:** "bu tool int döndürür, DECIDE'da int karşılaştırma yap"

Tip sistemi JSON Schema kullanır (Tool Registry ile aynı).

---

## 14. Web UI (Opsiyonel)

htmx-based dashboard (TinyOS'un mevcut dashboard deseni ile aynı):

- Graph editor: node'ları görsel düzenle
- Execution monitor: canlı execution takibi
- Branch tree: fork'ları görsel tree
- Audit: her execution step'i incele

---

## 15. Tam Mimari — tinypipe İzole Proje Yapısı

**Proje kökü:** `/home/roy/github-projects/tinypipe/`

```
tinypipe/                          # Bağımsız workspace (tinymachine gibi)
├── Cargo.toml                      # workspace root
├── tinypipe-ir/                   # FlatBuffers schema + opcode enum + types
│   ├── src/lib.rs
│   ├── schemas/execution_plan.fbs  # FlatBuffers schema
│   └── Cargo.toml
├── tinypipe-compiler/             # Frontend + Backend
│   ├── src/
│   │   ├── lib.rs
│   │   ├── frontend/
│   │   │   ├── mod.rs
│   │   │   ├── parse.rs            # rustpython_parser wrapper
│   │   │   ├── sanitizer.rs        # AST Sanitizer (Visitor)
│   │   │   ├── transform.rs        # Python AST → Opcode AST
│   │   │   ├── validate.rs         # Static Validation
│   │   │   ├── cfi.rs              # CFG Flattening
│   │   │   └── error.rs            # Compiler feedback (satır/sütun/sebep)
│   │   └── backend/
│   │       ├── mod.rs
│   │       ├── optimize.rs         # Constant folding + dead node + fusion
│   │       └── codegen.rs          # Opcode AST → FlatBuffers binary
│   └── Cargo.toml
├── tinypipe-vm/                   # DAG interpreter (zero-copy)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── plan.rs                 # FlatBuffers IR loader
│   │   ├── context.rs              # Memory model (Input/Working/Output)
│   │   ├── engine.rs               # Topolojik yürütücü
│   │   ├── budget.rs               # Node count + wall-clock + memory
│   │   ├── dispatch.rs             # CALL target dispatch (trait-based)
│   │   ├── parallel.rs             # PARALLEL + Local Scope Memory
│   │   ├── loop.rs                 # LOOP executor
│   │   ├── wait.rs                 # WAIT timer (v1: async, v2: durable)
│   │   ├── suspend.rs              # SUSPEND/RESUME (v3)
│   │   └── merge.rs                # MERGE strategies
│   └── Cargo.toml
├── tinypipe-api/                  # Public traits (TinyOS ve diğer tüketiciler için)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── tool_registry.rs        # ToolRegistry trait (CALL dispatch için)
│   │   ├── storage.rs              # Storage trait (SQLite abstraction)
│   │   └── types.rs                # Ortak tipler
│   └── Cargo.toml
├── tinypipe-storage/              # SQLite implementasyonu
│   ├── src/
│   │   ├── lib.rs
│   │   ├── graphs.rs               # CRUD + version management
│   │   ├── executions.rs           # Execution + steps
│   │   ├── scheduler.rs            # Durable execution scheduler
│   │   └── migrations.rs           # Schema migrations
│   └── Cargo.toml
├── tinypipe-cli/                  # CLI (bağımsız kullanım)
│   ├── src/main.rs
│   └── Cargo.toml
└── tools/
    └── build-initramfs.sh
```

### İzolasyon Sınırları

```
┌─────────────────────────────────────────────────────────────────┐
│                    tinypipe workspace                           │
│  ┌──────────┐  ┌──────────────┐  ┌──────────┐  ┌────────────┐  │
│  │  tinypipe│→│tinypipe     │→│tinypipe │→│tinypipe   │  │
│  │  -ir      │  │-compiler     │  │-vm       │  │-storage    │  │
│  └──────────┘  └──────────────┘  └─────┬────┘  └────────────┘  │
│                                        │                         │
│                               ┌────────▼────────┐               │
│                               │  tinypipe-api   │               │
│                               │  (traits only)   │               │
│                               └────────┬────────┘               │
│                                        │                         │
│       tinypipe hiçbir tinyos-* veya tinymachine-* import etmez  │
└────────────────────────────────────────┼─────────────────────────┘
                                         │
                                         ▼
                            ┌────────────────────────┐
                            │  tinyos (via path dep)  │
                            │                         │
                            │ tinyos-cli              │
                            │ tinyos-core             │
                            │ tinyos-orchestrator     │
                            │   ┌─────────────────┐  │
                            │   │ ToolRegistry    │  │
                            │   │ implementasyonu │──┼──→ tinymachine SandboxBackend
                            │   └─────────────────┘  │       (TinyOS çağırır,
                            └────────────────────────┘       tinypipe bilmez)
```

### Bağımlılık grafiği (Cargo.toml path'leri)

```
tinypipe-compiler ───→ tinypipe-ir
tinypipe-vm       ───→ tinypipe-ir
tinypipe-vm       ───→ tinypipe-api  (ToolRegistry trait)
tinypipe-storage  ───→ tinypipe-api  (Storage trait impl)
tinypipe-cli      ───→ tinypipe-compiler, tinypipe-vm, tinypipe-storage

# TinyOS tarafında:
tinyos-cli         ───→ ../../tinypipe/tinypipe-api   (path dep)
tinyos-core        ───→ ../../tinypipe/tinypipe-api   (path dep)
tinyos-orchestrator───→ ../../tinypipe/tinypipe-api   (path dep)
```

### tinypipe-api trait'leri (izolasyonun temeli)

```rust
// tinypipe-api/src/tool_registry.rs
pub trait ToolRegistry: Send + Sync {
    fn resolve(&self, name: &str, version: &str) -> Result<ToolSpec>;
    fn dispatch(&self, call: &CallTarget, context: &Context) -> Result<Value>;
    fn latest_schema_hash(&self, name: &str) -> Result<String>;
}

// tinypipe-api/src/storage.rs
pub trait GraphStorage: Send + Sync {
    fn create_graph(&self, name: &str, code: &str) -> Result<GraphId>;
    fn update_graph(&self, id: GraphId, code: &str) -> Result<Version>;
    fn deploy(&self, id: GraphId, version: Version) -> Result<()>;
    fn load_plan(&self, id: GraphId) -> Result<Vec<u8>>; // FlatBuffers blob
    fn save_execution(&self, exec: &Execution) -> Result<()>;
    fn save_step(&self, step: &ExecutionStep) -> Result<()>;
}

Bu trait'ler sayesinde:
- `tinypipe-vm` **TinyOS'u tanımaz** — sadece `ToolRegistry` trait'ini kullanır
- `tinypipe-storage` **TinyOS'u tanımaz** — sadece `GraphStorage` trait'ini implement eder
- TinyOS, bu trait'leri implemente ederek tinypipe'i kullanır
- TinyOS, `ToolRegistry::dispatch` implementasyonunda `tinymachine`'in `SandboxBackend`'ini çağırarak tool dispatch yapar
- Test'lerde mock implementasyonlar kullanılır

---

## 16. Uygulama Sırası (v1 + v2 + v3)

| Aşama | Süre | crate | Ne | Detay |
|-------|------|-------|----|-------|
| **v1.0** | 2 gün | `tinypipe-compiler` | **Restricted Python + Sanitizer** | `rustpython_parser` entegrasyonu, AST Sanitizer (Visitor), Python→Opcode AST transform, hata raporlama. (Sıfırdan Lexer/Parser yok — hazır kütüphane.) |
| **v1.1** | 3 gün | `tinypipe-vm` + `tinypipe-ir` + `tinypipe-storage` | Opcode sistemi (11 opcode + edges + context) | Opcode AST → Engine (interpreter), Static Validation, SQLite storage. LOOP memory growth uyarısı dahil. Wall-clock timeout (`max_execution_time_ms`) budget kontrolü dahil. |
| **v1.2** | 3 gün | `tinypipe-vm` + `tinypipe-api` | CALL target: tool + subgraph + partial failure | CALL dispatch, `on_error`/`fallback_value` parametreleri, ToolRegistry trait (TinyOS implemente eder). |
| **v1.3** | 3 gün | `tinypipe-vm` | WAIT v1 + Auto-Repair | max 300s async timer, compiler error → LLM feedback |
| **v2.0** | 1 hafta | `tinypipe-compiler` + `tinypipe-cli` | LLM integration | Sohbet → create_graph(python_code) / update_graph(python_code). LLM'den prompt engineering. |
| **v2.1** | 3 gün | `tinypipe-storage` | Branch explore + fork_graph | Branch tree, copy-on-write graph |
| **v2.2** | 2 gün | `tinypipe-vm` + `tinypipe-compiler` | Subgraph desteği + cycle detection | `call("subgraph:adi")`, Global Call Graph DFS ile cycle detection, Subgraph nesting depth validation |
| **v2.3** | 1 hafta | `tinypipe-compiler` (backend) + `tinypipe-ir` | **FlatBuffers Backend + Codegen** | Optimize, codegen (string→uint32 index mapping), compiler frontend/backend ayrımı, `tool_version_hash` embed. FlatBuffers schema field ID kararlılığı, `ExecutionPlan.version` IR version detection, VM Version Compatibility Matrix. |
| **v2.4** | 3 gün | `tinypipe-vm` + `tinypipe-ir` | Edge metadata + condition | Edge-based routing |
| **v2.5** | 3 gün | `tinypipe-compiler` + `tinypipe-ir` | Typed Variables | Symbol table, compile-time type check |
| **v2.6** | 2 gün | `tinypipe-vm` + `tinypipe-api` | **Tool Schema Validation** | VM runtime schema drift detection, ToolDep.schema_hash control, auto-repair trigger |
| **v3.0** | 1 hafta | `tinypipe-vm` + `tinypipe-storage` + `tinypipe-api` | **Durable Execution + SUSPEND/RESUME** | Paused state, scheduler, resume engine, human-in-the-loop (suspend/resume webhook), resume_token idempotency |
| **v3.1** | 3 gün | `tinypipe-vm` | Compensation / Saga | Compensate tool, rollback |
| **v3.2** | 1 hafta | `tinypipe-cli` | Web UI | Graph editor, execution monitor |

**Toplam: ~8 hafta (v1: ~1.5 hafta, v2: 3.5 hafta, v3: 3 hafta)** — Sıfırdan DSL/Parser yazmadığımız için v1 ~%40 kısaldı. Schema validation ve SUSPEND/RESUME mimari kritik olduğu için v2 + v3 arasına ek hafta eklendi.

---

## 17. Başarı Kriterleri

- İş birimi yeni bir ürünü geliştiriciye ihtiyaç duymadan çıkarabilmeli
- Her ürün değişikliği dakikalar içinde test edilip deploy edilebilmeli
- Production'da LLM çağrılmamalı, her execution deterministik olmalı
- Denetimde "bu müşteriye neden bu ürün önerildi?" sorusu execution_steps ile cevaplanabilmeli
- Aynı girdi her zaman aynı çıktıyı üretmeli (deterministik replay)
- Rollback en fazla 1 saniye sürmeli (pointer değiştirme)
- Compiler optimizasyonları büyük graph'larda (%50+ node) en az %20 performans kazancı sağlamalı
- FlatBuffers IR yükleme süresi JSON parse'ın %1'inden az olmalı (<1µs)
- FlatBuffers IR boyutu JSON'un %25'inden az olmalı
- **Code audit:** "Bu graph hangi kodla oluşturuldu?" sorusu Python kodu ile cevaplanabilmeli
- **Parser determinizmi:** Aynı Python kodu her zaman aynı AST'yi üretmeli (`rustpython_parser` testleri ile kanıtlanmıştır)
- **Auto-repair:** LLM'in ilk denemede başarısız olması durumunda en fazla 3 iterasyonda graph oluşturulabilmeli
- **Type inference:** CALL çıktı tipleri Tool Registry'den otomatik çıkarılmalı, tip hataları compile-time yakalanmalı
- **Tool version pinning:** IR'deki graph, kullandığı her tool'un semver constraint'ini saklamalı, tool breaking change'inden etkilenmemeli
- **Fusion:** Multi-Branch Fusion ve Calc Fusion, aynı graph'ın optimize edilmemiş haline göre en az %15 daha az node üretmeli
- **Execution budget:** `max_node_execution_count` aşıldığında VM hatayı `ExecutionBudgetExceeded` ile döndürmeli
- **Context memory limit:** `max_context_memory_bytes` aşıldığında VM `ContextMemoryExceeded` döndürmeli (OOM koruması)
- **Recursion limit:** `max_recursion_depth` aşıldığında VM `RecursionLimitExceeded` döndürmeli (sonsuz recursion koruması)
- **Subgraph cycle detection:** Global Call Graph DFS compile-time'da tüm subgraph'lar arasında döngü tespit etmeli, cycle varsa derleme hatası döndürmeli
- **Partial failure:** PARALLEL içinde `on_error="continue_with_null"` olan CALL başarısız olduğunda diğer branch'ler etkilenmemeli, MERGE'de null değeri ile birleşme yapılabilmeli
- **Tool schema validation:** `tool_version_hash` değiştiğinde VM `ToolSchemaChanged` hatası döndürmeli, geriye uyumlu şema değişikliklerinde (yeni optional field) devam edebilmeli
- **Human-in-the-loop:** SUSPEND sonrası execution `awaiting_approval` state'inde persist edilmeli, resume API ile devam edebilmeli, timeout'ta on_timeout stratejisine göre davranmalı
- **Codegen determinizm:** Aynı Opcode AST her zaman aynı FlatBuffers binary'i üretmeli (string→uint32 mapping deterministic — insertion order veya topolojik sıra)
- **Geriye dönük uyumluluk:** Yeni VM (vN+1), eski IR (vN) binary'lerini hatasız çalıştırabilmeli. Eski VM yeni IR'de bilmediği opcode ile karşılaşırsa `UnknownOpcode` hatası döndürmeli, crash olmamalı
- **Wall-clock timeout:** `max_execution_time_ms` aşıldığında VM `ExecutionTimeoutExceeded` döndürmeli, timeout süresi deterministik olmalı (aynı input aynı noktada timeout almalı)
- **Schema field ID kararlılığı:** FlatBuffers table field ID'leri (`@0`, `@1`, ...) asla değiştirilmemeli, silinen field'ların ID'leri `deprecated` olarak işaretlenmeli, asla yeniden kullanılmamalı

---

## 18. TinyOS ile Entegrasyon (Path Dependency Model)

### 18.1 Bağımlılık Yönü

```
TinyOS ──path dep──▶ tinypipe       (graph/compiler)
TinyOS ──path dep──▶ tinymachine    (sandbox/VM — ayrı, tinypipe üzerinden değil)
  (agent/UI)
```

- `tinypipe` **TinyOS'u tanımaz** (izole)
- `tinypipe` **tinymachine'i tanımaz** (izole) — tool dispatch `ToolRegistry` trait'i üzerinden TinyOS tarafından implemente edilir
- TinyOS, `tinypipe`'in trait'lerini implemente ederek kullanır
- TinyOS, `tinymachine`'i doğrudan da kullanır (fork engine, wasm sandbox)

### 18.2 TinyOS Cargo.toml Bağımlılıkları

```toml
# tinyos/Cargo.toml — workspace
[workspace]
members = [
    "tinyos-api",
    "tinyos-core",
    "tinyos-orchestrator",
    "tinyos-cli",
    "tinyos-memory",
    "tinyos-providers",
    "tinyos-tools",
    "tinyos-channels",
    # NOT: tinypipe workspace member DEĞİL, path dependency
]

# tinyos-api/Cargo.toml — tinypipe-api'yi re-export eder
[dependencies]
tinypipe-api = { path = "../../tinypipe/tinypipe-api" }

# tinyos-core/Cargo.toml — agent loop'ta tinypipe kullanımı
[dependencies]
tinypipe-api = { path = "../../tinypipe/tinypipe-api" }

# tinyos-orchestrator/Cargo.toml — durable execution için
[dependencies]
tinypipe-api = { path = "../../tinypipe/tinypipe-api" }
tinypipe-storage = { path = "../../tinypipe/tinypipe-storage" }
```

### 18.3 Trait Implementasyonları (TinyOS tarafı)

```rust
// tinyos-orchestrator/src/graph_integration.rs
use tinypipe_api::tool_registry::{ToolRegistry, CallTarget, ToolSpec};
use tinypipe_api::storage::GraphStorage;

struct TinyOsToolRegistry {
    tools: HashMap<String, ToolSpec>,
    tinymachine: Arc<tinymachine_fork::fork::ForkEngine>,
}

impl ToolRegistry for TinyOsToolRegistry {
    fn resolve(&self, name: &str, version: &str) -> Result<ToolSpec> {
        self.tools.get(name)
            .cloned()
            .ok_or_else(|| anyhow!("Tool '{}' not found", name))
    }
    
    fn dispatch(&self, call: &CallTarget, context: &Context) -> Result<Value> {
        // tinymachine üzerinden tool'u çalıştır
        match call.target {
            TargetType::Tool(name) => {
                let backend = self.tinymachine.acquire("python:minimal")?;
                backend.exec(&format!("tool_{}({})", name, call.params))  // simplified
            }
            TargetType::Subgraph(name) => {
                // subgraph: başka bir tinypipe graph'ını recursive çağır
                let sub_plan = self.storage.load_plan(name)?;
                let sub_vm = GraphVm::new(sub_plan);
                sub_vm.run(context)
            }
        }
    }
    
    fn latest_schema_hash(&self, name: &str) -> Result<String> {
        // Tool Registry'den güncel şema hash'ini al
        Ok(self.tools.get(name)
            .map(|t| t.schema_hash.clone())
            .unwrap_or_default())
    }
}
```

### 18.5 Dosya Organizasyonu

```
# tinypipe projesi (izole)
/home/roy/github-projects/tinypipe/
├── Cargo.toml
├── tinypipe-ir/
├── tinypipe-compiler/
├── tinypipe-vm/
├── tinypipe-api/
├── tinypipe-storage/
└── tinypipe-cli/

# tinyos projesi (tüketici)
/home/roy/github-projects/tinyos/
├── Cargo.toml
├── tinyos-api/          # re-export: tinypipe-api
├── tinyos-core/         # kullanır: tinypipe-api
├── tinyos-orchestrator/ # kullanır: tinypipe-api, tinypipe-storage
├── tinyos-cli/          # kullanır: tinypipe-cli benzeri komutlar
├── tinyos-memory/
├── tinyos-tools/        # ToolRegistry implementasyonu
└── tinyos-channels/

# tinymachine projesi (sandbox sağlayıcı)
/home/roy/github-projects/tinymachine/
├── Cargo.toml
├── tinymachine-api/     # SandboxBackend trait
├── tinymachine-fork/    # KVM + wasm sandbox
├── tinymachine-config/
├── tinymachine-ir/
└── tinymachine-cli/
```

### 18.6 Entegrasyon Tablosu

| tinypipe Bileşeni | TinyOS'ta Ne İşe Yarar | TinyOS Bağımlılığı |
|--------------------|------------------------|---------------------|
| `tinypipe-api` | ToolRegistry, GraphStorage trait'leri | `path = "../../tinypipe/tinypipe-api"` |
| `tinypipe-compiler` | Restricted Python → FlatBuffers IR derleme | TinyOS iplemez, CLI üzerinden kullanılır |
| `tinypipe-vm` | DAG interpreter (zero-copy) | TinyOS iplemez, storage'dan plan alır |
| `tinypipe-ir` | FlatBuffers schema + opcode enum | TinyOS iplemez, compiler/VM içindir |
| `tinypipe-storage` | SQLite persistent storage | `tinyos-orchestrator` kullanır (opsiyonel) |
| `tinypipe-cli` | create/update/deploy graph | `tinyos exec` benzeri komutlar |

### 18.7 tinymachine Entegrasyonu

tinypipe, **tinymachine'i tanımaz** ve ona bağımlı değildir. tinymachine entegrasyonu tamamen TinyOS tarafında gerçekleşir:

```
tinypipe-vm → ToolRegistry::dispatch(call) → Value
                       ▲
                       │ implements (TinyOsToolRegistry)
                       │
               tinyos-orchestrator
                       │
                       │ calls SandboxBackend::exec(code)
                       ▼
                 tinymachine-fork
```

TinyOS, `ToolRegistry::dispatch` implementasyonunda `tinymachine-api`'nin `SandboxBackend` trait'ini kullanarak tool kodlarını çalıştırır. Bu detay tinypipe'i ilgilendirmez — tinypipe sadece `ToolRegistry` trait'ini bilir.

> **Parser stratejisi:** tinypipe-compiler, Restricted Python parse etmek için direkt `rustpython-parser` kullanır
> (`tinymachine-ir` üzerinden değil). Her iki proje de aynı kütüphaneyi kullanır ancak bağımsızdır —
> tinymachine import analizi için, tinypipe Restricted Python → Opcode AST dönüşümü için.
> İleride ortak bağımlılık olarak düşünülebilir (her iki Cargo.toml'a ayrı ayrı eklenir).

---

## 19. Test & Benchmark Strategy

### 19.1 TinyGrad Pattern Adoption

| # | Pattern | TinyGrad | tinypipe | Priority |
|---|---------|----------|----------|----------|
| 1 | **Process Replay** | `process_replay.py` — SQLite-stored kernel output regression | Compile `ExecutionPlan`, store + diff on compiler/VM changes | **P0** |
| 2 | **Null tests** | `test/null/` — 55 files, no backend needed | `tests/null/` in `tinypipe-compiler` — parse → sanitize → validate → codegen | **P0** |
| 3 | **Backend tests** | `test/backend/` — same ops, every backend | `tests/backend/` in `tinypipe-vm` — same plan, any `ToolRegistry` impl | **P0** |
| 4 | **Graph fuzz** | `fuzz_graph.py` — random programs + ground truth | Generate random Restricted Python → compile → verify invariants (DAG, no crash, budget) | **P1** |
| 5 | **Test helpers** | `helpers.py` — `timeit()`, `assert_jit_cache_len()`, `eval_uop()` | `test_helpers.rs` — `test_graphs()`, `mock_registry()`, `assert_plan_eq()` | **P0** |
| 6 | **Property fuzz** | `fuzz_fast_idiv.py` — Z3 verification | Formal verification of compiler transforms | **P2** |

### 19.2 Test Layout (per crate)

```
tinypipe-compiler/
├── tests/
│   ├── null/                  # compile-only, no VM needed
│   │   ├── test_parse.rs      # successful parse + error cases
│   │   ├── test_sanitize.rs   # restricted Python rules (40+ cases)
│   │   ├── test_transform.rs  # desugar, simplify
│   │   ├── test_validate.rs   # output/errors correct
│   │   └── test_codegen.rs    # FlatBuffers serialization
│   └── replay/
│       └── references/        # committed reference plans
│           ├── basic/         # arithmetic, string, list
│           ├── control/       # if/for/while
│           └── errors/        # expected error plans
│
tinypipe-vm/
├── tests/
│   ├── backend/               # runs against any ToolRegistry
│   │   ├── test_execute.rs    # opcode-by-opcode execution
│   │   └── test_pipeline.rs   # compile → execute end-to-end
│   ├── unit/                  # single-backend (MockToolRegistry)
│   │   ├── test_budget.rs     # step/time budget enforcement
│   │   ├── test_errors.rs     # error propagation, cancellation
│   │   ├── test_context.rs    # context mutation, CALL/CALLBACK
│   │   └── test_replay.rs     # process replay regression
│   └── replay/
│       └── results/           # committed execution outputs
│
tinypipe-storage/
├── tests/
│   ├── test_graphs.rs         # CRUD for graph definitions
│   ├── test_executions.rs     # execution history
│   └── test_scheduler.rs      # scheduler edge cases
│
tinypipe-ir/
├── tests/
│   ├── test_opcodes.rs        # opcode enum invariants
│   ├── test_flatbuffers.rs    # round-trip serialization
│   └── test_types.rs          # type system invariants
│
tinypipe-api/
└── tests/
    ├── test_tool_registry.rs  # trait contract tests
    └── test_storage.rs        # trait contract tests
```

### 19.3 Key Scenarios

**Sanitizer (40+ cases)** — all in `test_sanitize.rs`:

```
Allowed:
  x = 1                             # CALC
  x + y                             # CALC with variables
  [1, 2, 3]                         # List literal
  if x > 0: pass                    # DECIDE
  for i in range(10): pass          # LOOP
  def f(x): return x+1              # function def
  lambda x: x+1                     # lambda
  call("tool", arg=x)               # CALL
  act("TYPE", msg="hello")          # ACT
  with parallel() as p: ...         # PARALLEL
  return x                          # OUTPUT

Blocked:
  import os                         # Import
  from tools import *               # ImportFrom
  os.system("rm -rf /")             # arbitrary call
  subprocess.run(...)               # arbitrary call
  open("/etc/passwd")               # arbitrary call
  eval("...")                       # eval
  exec("...")                       # exec
  __import__("os")                  # dunder import
  getattr(obj, "__class__")         # reflection
  class Foo: ...                    # class def
  while True: ...                   # while (sonsuz döngü)
  yield / async                     # async/generator
  sys._getframe()                   # stack inspection
  try: ... except: ...              # try (graph ERROR kullan)
  f"hello {x}"                      # f-string (template syntax kullan)
```

**Opcode isolation tests** — each opcode in `test_execute.rs`:
- `LOAD_CONST` with every literal type (int, float, string, bool, null, list, dict)
- `BINARY_OP` with overflow/underflow per type, division by zero, type mismatch
- `COMPARE_OP` at type boundaries (NaN, Inf, large ints across bit widths)
- `CALL` — arity mismatch, type mismatch, undefined function, timeout simulation
- `CALLBACK` — callback timeout, cancel, error return from callback
- `CONTROL_FLOW` — nested if/for/while, break/continue edge cases, unreachable branches
- `SUSPEND_OP` (v3) — sleep duration, cancellation during sleep
- `PARALLEL` — branch counts (0, 1, N), partial failure in one branch, all branches fail
- `MERGE` — all/any/last modes, empty branches, field conflicts, null handling

**Budget enforcement** — all in `test_budget.rs`:
- Step budget exceeded → `ExecutionError::BudgetExceeded`
- Time budget exceeded → `ExecutionError::Timeout`
- Budget applies per graph, not per step
- CALL actions decrement from parent's budget
- PARALLEL branches each consume from shared budget
- LOOP iterations stack: 100 iterations × 5 nodes = 500 against budget

**Process replay** — `test_replay.rs`:

```rust
fn test_replay_arithmetic() {
    let plan = load_reference("basic/add.i64");  // committed reference
    let result = execute(&plan, &mut MockToolRegistry::new());
    assert_eq!(result, expected);
}
```

- Reference data in `tests/replay/references/` and `tests/replay/results/`
- CI runs `update-references` mode to regenerate on intentional changes
- PR must include updated references or CI fails
- Replay covers both compiler output (plan bytes identical) and VM output (result identical)

### 19.4 Benchmark Strategy

| Benchmark | Crate | What it measures | Target |
|-----------|-------|------------------|--------|
| `compile_parse` | compiler | rustpython_parser: 10-node graph code | `<20µs` |
| `compile_sanitize` | compiler | sanitize + validate a 10-node graph AST | `<30µs` |
| `compile_e2e` | compiler | parse → codegen for a 10-node graph | `<100µs` |
| `compile_large` | compiler | full pipeline: 1000-line script | `<1ms` |
| `execute_e2e` | vm | execute a 10-node plan on MockToolRegistry | `<50µs` |
| `execute_many_calls` | vm | 1000 CALL actions | `<5ms` |
| `execute_parallel` | vm | PARALLEL with 10 branches, 10 nodes each | `<500µs` |
| `execute_loop` | vm | LOOP with 1000 iterations, 5 nodes/iteration | `<5ms` |
| `serialize_roundtrip` | ir | FlatBuffers encode + decode | `<1µs` |
| `storage_crud_1000` | storage | 1000 graph CRUD ops | `<100ms` |
| `vm_budget_check` | vm | budget.check() microbenchmark | `<100ns` |
| `compile_fuzz` | compiler | fuzz-generated ASTs (stability, not speed) | no crash |

**Bench harness:**

```rust
// Custom bench harness (no criterion dependency to keep binary small)
pub struct BenchStats {
    pub count: usize,
    pub mean: f64,     // μs
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

pub fn run_bench<F: FnMut()>(name: &str, mut f: F, iterations: usize) -> BenchStats {
    // 1. Warmup: 10 iterations (JIT, cache warm)
    for _ in 0..10 { f(); }

    // 2. Measure: N iterations, record each
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = std::time::Instant::now();
        f();
        let elapsed = start.elapsed().as_nanos() as f64 / 1000.0; // μs
        samples.push(elapsed);
    }

    // 3. Sort for percentiles
    samples.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

    let count = samples.len();
    let mean = samples.iter().sum::<f64>() / count as f64;
    let min = samples[0];
    let max = samples[count - 1];
    let p50 = samples[count / 2];
    let p95 = samples[(count as f64 * 0.95) as usize];
    let p99 = samples[(count as f64 * 0.99) as usize];

    BenchStats { count, mean, min, max, p50, p95, p99 }
}
```

- Benchmarks in `benches/*.rs` per crate, `harness = false` in Cargo.toml
- Baseline stored as JSON in `benches/baseline/`
- CI detects regression >5% vs baseline → FAIL
- `cargo bench -- --save-baseline` to update baseline (manual, intentional)

### 19.5 Mock Infrastructure

```rust
// tinypipe-vm/tests/mocks/mock_registry.rs

pub struct MockToolRegistry {
    tools: HashMap<String, MockTool>,
}

pub struct MockTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub exec: Box<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>,
}

impl ToolRegistry for MockToolRegistry {
    fn resolve(&self, name: &str, version: &str) -> Result<ToolSpec, RegistryError> {
        self.tools.get(name)
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
                pure: false,
                version: "0.0.0".into(),
                schema_hash: "mock".into(),
            })
            .ok_or(RegistryError::NotFound(name.into()))
    }

    fn dispatch(&self, call: &CallTarget, _context: &Context) -> Result<Value, DispatchError> {
        let tool = self.tools.get(&call.name)
            .ok_or(DispatchError::NotFound(call.name.clone()))?;
        (tool.exec)(&call.args).map_err(|e| DispatchError::ExecutionFailed(e))
    }

    fn latest_schema_hash(&self, name: &str) -> Result<String, RegistryError> {
        Ok("mock".into())
    }
}
```

**Default tools for testing (`mock_tools()` factory):**

| Tool | Behaviour | Use |
|------|-----------|-----|
| `math.add(a, b)` | returns `a + b` | basic CALL |
| `math.mul(a, b)` | returns `a * b` | chained CALLs |
| `string.len(s)` | returns `s.len()` | type variance |
| `echo(val)` | returns `val` unchanged | passthrough / idempotency |
| `test.sleep(ms)` | sleeps for `ms` ms | budget tests, timeout |
| `test.error(msg)` | always returns `Err(msg)` | partial failure, `on_error` |
| `test.callback(id)` | invokes callback with `id` | CALLBACK opcode |
| `test.large(n)` | returns `"x".repeat(n)` | context memory limit |
| `test.delay_schema()` | returns different schema on 2nd call | schema drift simulation |

```rust
pub fn mock_tools() -> MockToolRegistry {
    let mut reg = MockToolRegistry::new();

    reg.add("math.add", |args| {
        let a = args[0].as_f64().ok_or("not a number")?;
        let b = args[1].as_f64().ok_or("not a number")?;
        Ok(Value::Number((a + b).into()))
    });

    reg.add("test.sleep", |args| {
        let ms = args[0].as_u64().ok_or("not a u64")?;
        std::thread::sleep(Duration::from_millis(ms));
        Ok(Value::Null)
    });

    reg.add("test.error", |_args| {
        Err("simulated tool error".into())
    });

    reg
}
```

### 19.6 Dev-Dependencies

```toml
# tinypipe-compiler/Cargo.toml
[dev-dependencies]
tempfile = "3"          # temporary directories for test artifacts
serde_json = "1"        # test fixtures
tinypipe-ir = { path = "../tinypipe-ir" }  # opcode types for test assertions

# tinypipe-vm/Cargo.toml
[dev-dependencies]
tempfile = "3"
serde_json = "1"
tinypipe-ir = { path = "../tinypipe-ir" }
tinypipe-api = { path = "../tinypipe-api" }

# tinypipe-storage/Cargo.toml
[dev-dependencies]
tempfile = "3"
serde_json = "1"

# tinypipe-ir/Cargo.toml
[dev-dependencies]
serde_json = "1"

# tinypipe-api/Cargo.toml
[dev-dependencies]
serde_json = "1"
```

### 19.7 Implementation Order

| Step | What | Crate | Testing Focus |
|------|------|-------|---------------|
| 1 | Opcode types + FlatBuffers schema | `tinypipe-ir` | `test_opcodes.rs` — enum invariants, `test_flatbuffers.rs` — round-trip |
| 2 | MockToolRegistry helpers | `tinypipe-vm` | `tests/mocks/mock_registry.rs` — shared across vm tests |
| 3 | Sanitizer (40+ rules) | `tinypipe-compiler` | `tests/null/test_sanitize.rs` — each rule a test case |
| 4 | Parse + Transform | `tinypipe-compiler` | `tests/null/test_parse.rs`, `test_transform.rs` |
| 5 | Static Validation + CFG Flattening | `tinypipe-compiler` | `tests/null/test_validate.rs` — cycle, terminal, input |
| 6 | Codegen | `tinypipe-compiler` | `tests/null/test_codegen.rs` — binary output |
| 7 | VM: basic exec + all 11 opcodes | `tinypipe-vm` | `tests/backend/test_execute.rs` — opcode isolation |
| 8 | VM: budget enforcement | `tinypipe-vm` | `tests/unit/test_budget.rs` — step/time/memory limits |
| 9 | VM: error propagation | `tinypipe-vm` | `tests/unit/test_errors.rs` — CALL failures, on_error modes |
| 10 | Storage CRUD + migrations | `tinypipe-storage` | `tests/test_graphs.rs`, `test_executions.rs` |
| 11 | Process replay setup | all | `tests/replay/` — reference data + `test_replay.rs` |
| 12 | Benchmarks | all | `benches/*.rs` — baseline + regression detection |
| 13 | Graph fuzz | `tinypipe-compiler` | Fuzz: random ASTs → compile → no crash |
