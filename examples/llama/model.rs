#![allow(clippy::type_complexity)]
use std::ops::{Add, Mul};

use luminal::{
    nn::{activation::RMSNorm, embedding::Embedding, linear::Linear},
    op::{self, ReshapeDim},
    prelude::{movement::TryConcatAlong, *},
};
use rand::{thread_rng, Rng};

// Full LLaMa model implementation, heavily based off of https://github.com/coreylowman/llama-dfdx/blob/main/src/modeling.rs

pub struct Mlp<const I: usize, const H: usize> {
    pub gate_proj: GraphTensor<(Const<I>, Const<H>)>,
    pub down_proj: GraphTensor<(Const<H>, Const<I>)>,
    pub up_proj: GraphTensor<(Const<I>, Const<H>)>,
}

impl<const I: usize, const H: usize, B: Dim, S: Dim> Module<GraphTensor<(B, S, Const<H>)>>
    for Mlp<I, H>
{
    type Output = GraphTensor<(B, S, Const<H>)>;

    fn forward(&self, input: GraphTensor<(B, S, Const<H>)>) -> Self::Output {
        let gate = input.matmul(self.gate_proj.permute());
        let gate = gate.sigmoid() * gate;
        let up = input.matmul(self.up_proj.permute()) * gate;
        up.matmul(self.down_proj.permute())
    }
}

impl<const I: usize, const H: usize> InitModule for Mlp<I, H> {
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            gate_proj: cx.new_tensor("Weight"),
            up_proj: cx.new_tensor("Weight"),
            down_proj: cx.new_tensor("Weight"),
        }
    }
}

impl<const I: usize, const H: usize> SerializeModule for Mlp<I, H> {
    fn serialize(&self, s: &mut Serializer) {
        s.tensor("gate", self.gate_proj);
        s.tensor("up", self.up_proj);
        s.tensor("down", self.down_proj);
    }
}

pub struct RotaryEmbedding<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize> {
    pub inv_freq: GraphTensor<R1<HEAD_DIM_OVER_2>>,
}

impl<Batch: Dim, NumHeads: Dim, Seq: Dim, const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize>
    Module<(
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
        usize,
    )> for RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = (
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    );

    fn forward(
        &self,
        (q, k, offset): (
            GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
            GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
            usize,
        ),
    ) -> Self::Output {
        let (sin, cos) = self.get_sincos(offset, q);
        let sin = sin.expand();
        let cos = cos.expand();
        let q_embed = (Self::rotate_half(q) * sin) + (q * cos);
        let k_embed = (Self::rotate_half(k) * sin) + (k * cos);
        (q_embed, k_embed)
    }
}

impl<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize>
    RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn get_sincos<Batch: Dim, NumHeads: Dim, Seq: Dim>(
        &self,
        offset: usize,
        seq_tensor: GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    ) -> (
        GraphTensor<(Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Seq, Const<HEAD_DIM>)>,
    ) {
        let graph = unsafe { self.inv_freq.graph_ref.as_mut().unwrap() };
        let t: GraphTensor<(Seq,)> = GraphTensor::from_id(
            graph
                .add_op(
                    op::Function(
                        "ARange".to_string(),
                        Box::new(move |inp, i| {
                            (
                                Some(Tensor {
                                    data: Box::new(
                                        (0..inp[0].1.shape.shape()[0])
                                            .map(|i| i as f32)
                                            .collect::<Vec<_>>(),
                                    ),
                                }),
                                TensorView {
                                    tensor_id: i,
                                    shape: ShapeTracker::new(vec![inp[0].1.shape.shape()[0]]),
                                },
                            )
                        }),
                    ),
                    vec![Seq::const_size()],
                )
                .input(seq_tensor.id)
                .finish(),
            graph,
        ) + offset as f32;
        let freqs = t
            .expand::<(Seq, Const<HEAD_DIM>), _>()
            .matmul(self.inv_freq.expand())
            .realize::<(Seq, usize)>();
        let emb = (freqs, freqs).concat_along(Axis::<1>);
        (emb.sin().realize(), emb.cos().realize())
    }

    fn rotate_half<Batch: Dim, NumHeads: Dim, Seq: Dim>(
        x: GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    ) -> GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)> {
        let x1 = x.slice((.., .., .., ..HEAD_DIM_OVER_2));
        let x2 = x.slice((.., .., .., HEAD_DIM_OVER_2..));
        (-x2, x1).concat_along(Axis::<3>).realize()
    }
}

impl<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize> InitModule
    for RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn initialize(cx: &mut Graph) -> Self {
        let s = Self {
            inv_freq: cx.new_tensor("Inv Freq"),
        };
        // Init weight as uniform(-1, 1)
        let mut rng = thread_rng();
        s.inv_freq.set(
            (0..HEAD_DIM_OVER_2)
                .map(|_| rng.gen_range(-1_f32..1_f32))
                .collect::<Vec<_>>(),
        );
        s
    }
}

impl<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize> SerializeModule
    for RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn serialize(&self, s: &mut Serializer) {
        s.tensor("inv_freq", self.inv_freq);
    }
}

pub struct Attention<
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
> {
    pub q_proj: GraphTensor<(Const<HIDDEN>, Const<HIDDEN>)>,
    pub k_proj: GraphTensor<(Const<HIDDEN>, Const<HIDDEN>)>,
    pub v_proj: GraphTensor<(Const<HIDDEN>, Const<HIDDEN>)>,
    pub o_proj: GraphTensor<(Const<HIDDEN>, Const<HIDDEN>)>,
    pub rotary_embed: RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>,
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        Batch: Dim,
        CurSeq: Dim,
    >
    Module<(
        GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
        GraphTensor<(CurSeq, CurSeq)>,
        usize,
    )> for Attention<NUM_HEADS, HIDDEN, HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>;

    fn forward(
        &self,
        (x, attn_mask, past_seq): (
            GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
            GraphTensor<(CurSeq, CurSeq)>,
            usize,
        ),
    ) -> Self::Output {
        let q = x
            .matmul(self.q_proj.permute())
            .dyn_reshape::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>)>(vec![
                match Batch::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(0),
                },
                match CurSeq::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(1),
                },
                ReshapeDim::Const(NUM_HEADS),
                ReshapeDim::Const(HEAD_DIM),
            ])
            .permute::<_, Axes4<0, 2, 1, 3>>();

        let k = x
            .matmul(self.k_proj.permute())
            .dyn_reshape::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>)>(vec![
                match Batch::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(0),
                },
                match CurSeq::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(1),
                },
                ReshapeDim::Const(NUM_HEADS),
                ReshapeDim::Const(HEAD_DIM),
            ])
            .permute::<_, Axes4<0, 2, 1, 3>>();

        let v = x
            .matmul(self.v_proj.permute())
            .dyn_reshape::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>)>(vec![
                match Batch::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(0),
                },
                match CurSeq::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(1),
                },
                ReshapeDim::Const(NUM_HEADS),
                ReshapeDim::Const(HEAD_DIM),
            ])
            .permute::<_, Axes4<0, 2, 1, 3>>();

        let (q, k) = self.rotary_embed.forward((
            q.realize::<(Batch, Const<NUM_HEADS>, CurSeq, Const<HEAD_DIM>)>(),
            k.realize(),
            past_seq,
        ));
        let inv_head_scale = (HEAD_DIM as f64).sqrt().recip() as f32;
        let w = q
            .batch_matmul(k.permute())
            .mul(inv_head_scale)
            .add(attn_mask.expand())
            .softmax::<3>();

        let o = w
            .batch_matmul(v)
            .permute::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>), _>()
            .dyn_reshape::<(Batch, CurSeq, Const<HIDDEN>)>(vec![
                match Batch::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(0),
                },
                match CurSeq::const_size() {
                    RealDim::Const(n) => ReshapeDim::Const(n),
                    RealDim::Dyn => ReshapeDim::PrevDim(1),
                },
                ReshapeDim::Const(HIDDEN),
            ]);

        o.matmul(self.o_proj.permute())
    }
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
    > InitModule for Attention<NUM_HEADS, HIDDEN, HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            q_proj: cx.new_tensor("Weight"),
            k_proj: cx.new_tensor("Weight"),
            v_proj: cx.new_tensor("Weight"),
            o_proj: cx.new_tensor("Weight"),
            rotary_embed: InitModule::initialize(cx),
        }
    }
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
    > SerializeModule for Attention<NUM_HEADS, HIDDEN, HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn serialize(&self, s: &mut Serializer) {
        s.tensor("q_proj", self.q_proj);
        s.tensor("k_proj", self.k_proj);
        s.tensor("v_proj", self.v_proj);
        s.tensor("o_proj", self.o_proj);
        s.module("rotary", &self.rotary_embed);
    }
}

pub struct DecoderLayer<
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const INTERMEDIATE: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
> {
    pub self_attn: Attention<NUM_HEADS, HIDDEN, HEAD_DIM, HEAD_DIM_OVER_2>,
    pub mlp: Mlp<INTERMEDIATE, HIDDEN>,
    pub input_layer_norm: RMSNorm<HIDDEN>,
    pub post_attention_layer_norm: RMSNorm<HIDDEN>,
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        Batch: Dim,
        CurSeq: Dim,
    >
    Module<(
        GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
        GraphTensor<(CurSeq, CurSeq)>,
        usize,
    )> for DecoderLayer<NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>;
    fn forward(
        &self,
        (x, attn_mask, past_seq_size): (
            GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
            GraphTensor<(CurSeq, CurSeq)>,
            usize,
        ),
    ) -> Self::Output {
        let y =
            self.self_attn
                .forward((self.input_layer_norm.forward(x), attn_mask, past_seq_size));
        let x = x + y;
        let y = self.mlp.forward(self.post_attention_layer_norm.forward(x));
        x + y
    }
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
    > InitModule for DecoderLayer<NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            self_attn: InitModule::initialize(cx),
            mlp: InitModule::initialize(cx),
            input_layer_norm: InitModule::initialize(cx),
            post_attention_layer_norm: InitModule::initialize(cx),
        }
    }
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
    > SerializeModule for DecoderLayer<NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn serialize(&self, s: &mut Serializer) {
        s.module("self_attn", &self.self_attn);
        s.module("mlp", &self.mlp);
        s.module("input_layer_norm", &self.input_layer_norm);
        s.module("post_attention_layer_norm", &self.post_attention_layer_norm);
    }
}

pub struct Llama<
    const VOCAB: usize,
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const INTERMEDIATE: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
    const LAYERS: usize,
> {
    pub embed_tokens: Embedding<VOCAB, HIDDEN>,
    pub layers: Vec<DecoderLayer<NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>>,
    pub norm: RMSNorm<HIDDEN>,
    pub graph_ref: *mut Graph,
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        const LAYERS: usize,
        Batch: Dim,
        CurSeq: Dim,
    > Module<(GraphTensor<(Batch, CurSeq)>, usize)>
    for Llama<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>;
    fn forward(
        &self,
        (input, past_seq_len): (GraphTensor<(Batch, CurSeq)>, usize),
    ) -> Self::Output {
        let graph = unsafe { self.graph_ref.as_mut().unwrap() };
        let attn_mask: GraphTensor<(CurSeq, CurSeq)> = GraphTensor::from_id(
            graph
                .add_op(
                    op::Function(
                        "AttentionMask".to_string(),
                        Box::new(|inp, i| {
                            let seq_len = inp[0].1.shape.shape()[1];
                            let mut data = vec![0.; seq_len * seq_len];
                            for i in 0..seq_len {
                                for j in (i + 1)..seq_len {
                                    data[i * seq_len + j] = f32::NEG_INFINITY;
                                }
                            }
                            (
                                Some(Tensor {
                                    data: Box::new(data),
                                }),
                                TensorView {
                                    tensor_id: i,
                                    shape: ShapeTracker::new(vec![
                                        inp[0].1.shape.shape()[1],
                                        inp[0].1.shape.shape()[1],
                                    ]),
                                },
                            )
                        }),
                    ),
                    vec![CurSeq::const_size(), CurSeq::const_size()],
                )
                .input(input.id)
                .finish(),
            graph,
        );

        let mut hidden_states = self.embed_tokens.forward(input);
        for layer_i in &self.layers {
            hidden_states = layer_i.forward((hidden_states, attn_mask, past_seq_len));
        }
        self.norm.forward(hidden_states)
    }
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        const LAYERS: usize,
    > InitModule
    for Llama<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>
{
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            norm: InitModule::initialize(cx),
            embed_tokens: InitModule::initialize(cx),
            layers: (0..LAYERS).map(|_| InitModule::initialize(cx)).collect(),
            graph_ref: cx,
        }
    }
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        const LAYERS: usize,
    > SerializeModule
    for Llama<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>
{
    fn serialize(&self, s: &mut Serializer) {
        s.module("norm", &self.norm);
        s.module("embed", &self.embed_tokens);
        for (i, l) in self.layers.iter().enumerate() {
            s.module(&format!("layer{i}"), l);
        }
    }
}

pub struct LlamaForCausalLM<
    const VOCAB: usize,
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const INTERMEDIATE: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
    const LAYERS: usize,
> {
    pub llama: Llama<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>,
    pub lm_head: Linear<HIDDEN, VOCAB>,
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        const LAYERS: usize,
        Batch: Dim,
        CurSeq: Dim,
    > Module<(GraphTensor<(Batch, CurSeq)>, usize)>
    for LlamaForCausalLM<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<VOCAB>)>;
    fn forward(
        &self,
        (input, past_seq_len): (GraphTensor<(Batch, CurSeq)>, usize),
    ) -> Self::Output {
        let hidden_states = self.llama.forward((input, past_seq_len));
        self.lm_head.forward(hidden_states)
    }
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        const LAYERS: usize,
    > InitModule
    for LlamaForCausalLM<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>
{
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            llama: InitModule::initialize(cx),
            lm_head: InitModule::initialize(cx),
        }
    }
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        const LAYERS: usize,
    > SerializeModule
    for LlamaForCausalLM<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2, LAYERS>
{
    fn serialize(&self, s: &mut Serializer) {
        s.module("llama", &self.llama);
        s.module("lm_head", &self.lm_head);
    }
}