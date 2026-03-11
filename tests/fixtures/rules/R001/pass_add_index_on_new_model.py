# CreateModel + AddIndex on same model - should pass (exempt)
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0002'),
    ]

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.BigAutoField(auto_created=True, primary_key=True)),
                ('name', models.CharField(max_length=255)),
                ('price', models.DecimalField(max_digits=10, decimal_places=2)),
                ('created_at', models.DateTimeField(auto_now_add=True)),
            ],
        ),
        migrations.AddIndex(
            model_name='product',
            index=models.Index(fields=['created_at'], name='product_created_idx'),
        ),
    ]
