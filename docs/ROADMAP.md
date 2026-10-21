# Yol haritası

Bloomberg Terminal karşılaştırmasından çıkan, piyasa takibi odaklı plan. İlgi alanları: dünya ve
Türkiye finansal gelişmeleri ve piyasaları, kripto, ABD ve BIST, piyasaları etkileyecek haberler.

Efor: **S** 1–2 gün, **M** 3–5 gün, **L** 1–2 hafta. Kaynaklar ücretsiz; ✓ ile işaretlenenler
Ekim 2026'da erişilebilir olduğu denenmiş olanlar.

## P0 — İlk sprintler

| # | Özellik | Bloomberg | Kaynak | Efor |
|---|---|---|---|---|
| 1 | **TOP haber akışı**: Dünya / Türkiye / Kripto / Merkez Bankaları sekmeleri, izleme listesi vurgusu, panelde okuma | TOP, NI | CNBC ✓, MarketWatch ✓, Fed ✓, ECB ✓, Bloomberg HT ✓, AA Ekonomi ✓, The Block ✓, CoinDesk ✓ | M |
| 2 | **KAP bildirim akışı** (`KAP`, `THYAO KAP`): özel durum, finansal rapor, temettü, sermaye artırımı, geri alım | CN (BIST) | KAP JSON uçları (araştırma gerekli) | M–L |
| 3 | **Anahtar kelime haber alarmları** | NSE alert | 1 üzerine | S |
| 4 | **Formüllü grafikler**: `XU100/USDTRY`, gram altın, `BTCUSD*USDTRY`, `SPX/XU100` | Özel endeks / spread | Mevcut veri | S–M |
| 5 | **Küresel ECO**: EUR/GBP/JPY/CNY… olayları, önem ve beklenti | ECO (World) | ForexFactory beslemesi (zaten çekiliyor) | S |
| 6 | **WEI Türkiye bloğu**: BIST sektör endeksleri, EURTRY, gram altın, dolar bazlı XU100, USDT/TRY | WEI | Yahoo `.IS`, BtcTurk ✓ | S |
| 7 | **Telefona bildirim** (ntfy / Telegram) | Anywhere alarmları | Ücretsiz | S |
| 8 | **Türkçe dil desteği**: arayüz dili seçimi (`LANG TR` / `LANG EN`, Ayarlar), menüler, panel başlıkları, düğmeler, durum ve hata metinleri, tarih/sayı biçimi (`1.234,56`, `22 Eki 2026`); fonksiyon kodları (`WEI`, `ECO`…) ve ticker'lar değişmez | — | — | M–L |

## P1 — Ana boşluklar

| # | Özellik | Bloomberg | Kaynak | Efor |
|---|---|---|---|---|
| 9 | **Türkiye makro paneli**: politika faizi, TÜFE/ÜFE, Piyasa Katılımcıları Anketi, reel faiz, rezervler, haftalık yabancı portföy akımları, mevduat faizi, kredi büyümesi | ECST, CENB | TCMB EVDS3 (ücretsiz anahtar) | M–L |
| 10 | **Türkiye ekonomi takvimi**: TÜFE, PPK, ödemeler dengesi, GSYH, işsizlik, güven endeksleri | ECO (TR) | TÜİK takvimi, PPK tarihleri, EVDS | M |
| 11 | **Merkez bankası monitörü** ve Fed için piyasa fiyatlı faiz yolu | WIRP | Yahoo ZQ vadelileri | M |
| 12 | **Kripto genel bakış** (`CRYP`): piyasa değeri, dominans, stablecoin, Fear & Greed, fonlama, OI, likidasyon, movers, USDT/TRY primi | CRYP | CoinGecko ✓, alternative.me ✓, Binance vadeli ✓, BtcTurk ✓ | M |
| 13 | **BIST piyasa paneli**: movers, `HEAT BIST`, yabancı payı, temettü / bedelsiz / halka arz takvimi | MOST, MMAP | Yahoo `.IS`, İş Yatırım, KAP | M |
| 14 | **Sabah brifingi** (`BRIEF`) | BRIEF, Launchpad | 1, 2, 5, 9 birleşimi | M |

## P2 — Derinlik

| # | Özellik | Efor |
|---|---|---|
| 15 | AI katmanı (Claude API, isteğe bağlı): haber etki skoru, KAP/8-K Türkçe özet, BRIEF özeti, "neden hareket ediyor?" | M |
| 16 | Grafikte haber / KAP / ECO / FOMC / PPK işaretleri | S–M |
| 17 | Tarayıcı (EQS) ABD + BIST | M–L |
| 18 | ABD piyasa genişliği, seans öncesi/sonrası movers | M |
| 19 | Analist önerisi ve hedef fiyat (ANR) | S |
| 20 | Portföyün TL bazlı ve enflasyondan arındırılmış getirisi | S |
| 21 | Küresel tahvil ve ülke riski (DE/UK/JP/IT 10Y, TR 2Y/10Y, TR CDS) — kaynak riskli | L |

## Kapsam dışı

Gerçek zamanlı BIST (lisans), AKD/takas, gerçek zamanlı CDS, konferans dökümleri, tahvil
fiyatlaması, IB sohbeti, VİOP (ücretsiz kaynak bulunamadı).

## Sıra

1. Parti 1 (paralel): 1, 2, 4, 5, 12
2. Parti 2: 3, 6, 7, 11, 9
3. Parti 3: 8 (Türkçe; tüm görünüm dosyalarına dokunduğu için diğerleri birleştikten sonra), 10, 13
4. Parti 4: 14, 15, 16 ve kalan P2
